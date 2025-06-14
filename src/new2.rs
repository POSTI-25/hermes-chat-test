use clap::Parser;
use futures::{executor::block_on, future::FutureExt, stream::StreamExt};
use libp2p::{
    core::multiaddr::{Multiaddr, Protocol},
    dcutr, gossipsub, identify, identity, noise, ping, relay,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, PeerId,
};
use std::{
    collections::hash_map::DefaultHasher,
    error::Error,
    hash::{Hash, Hasher},
    str::FromStr,
    time::Duration,
};
use tokio::{io, io::AsyncBufReadExt, select};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "libp2p DCUtR client with chat")]
struct Opts {
    /// The mode (client-listen, client-dial).
    #[arg(long)]
    mode: Mode,

    /// Fixed value to generate deterministic peer id.
    #[arg(long)]
    secret_key_seed: u8,

    /// The listening address of the relay server.
    #[arg(long)]
    relay_address: Multiaddr,

    /// Peer ID of the remote peer to hole punch to.
    #[arg(long)]
    remote_peer_id: Option<PeerId>,
}

#[derive(Clone, Debug, PartialEq, Parser)]
enum Mode {
    Dial,
    Listen,
}

impl FromStr for Mode {
    type Err = String;
    fn from_str(mode: &str) -> Result<Self, Self::Err> {
        match mode {
            "dial" => Ok(Mode::Dial),
            "listen" => Ok(Mode::Listen),
            _ => Err("Expected either 'dial' or 'listen'".to_string()),
        }
    }
}

// We create a custom network behaviour that combines DCUtR, Relay, Identify, Ping, and Gossipsub.
#[derive(NetworkBehaviour)]
struct Behaviour {
    relay_client: relay::client::Behaviour,
    ping: ping::Behaviour,
    identify: identify::Behaviour,
    dcutr: dcutr::Behaviour,
    gossipsub: gossipsub::Behaviour,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    let opts = Opts::parse();
    let keypair = generate_ed25519(opts.secret_key_seed);

    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(keypair.clone())
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_dns()?
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(|key, relay_behaviour| {
            // Content-address messages by hashing them.
            let message_id_fn = |message: &gossipsub::Message| {
                let mut s = DefaultHasher::new();
                message.data.hash(&mut s);
                gossipsub::MessageId::from(s.finish().to_string())
            };

            // Set a custom gossipsub configuration.
            let gossipsub_config = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(10))
                .validation_mode(gossipsub::ValidationMode::Strict)
                .message_id_fn(message_id_fn)
                .build()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

            // Build a gossipsub network behaviour.
            let gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
            )?;

            Ok(Behaviour {
                relay_client: relay_behaviour,
                ping: ping::Behaviour::new(ping::Config::new()),
                identify: identify::Behaviour::new(identify::Config::new(
                    "/chat/1.0.0".to_string(),
                    key.public(),
                )),
                dcutr: dcutr::Behaviour::new(key.public().to_peer_id()),
                gossipsub,
            })
        })?
        .build();

    // Create a Gossipsub topic and subscribe to it.
    let topic = gossipsub::IdentTopic::new("dcutr-chat-example");
    swarm.behaviour_mut().gossipsub.subscribe(&topic)?;

    // Listen on all interfaces.
    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()?)?;
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    // Wait to listen on all interfaces and print addresses.
    block_on(async {
        let mut delay = futures_timer::Delay::new(std::time::Duration::from_secs(1)).fuse();
        loop {
            futures::select! {
                event = swarm.next() => {
                    if let Some(SwarmEvent::NewListenAddr { address, .. }) = event {
                        println!("Local node is listening on {address}");
                    } else {
                        break;
                    }
                }
                _ = delay => {
                    break;
                }
            }
        }
    });

    // Connect to the relay server to learn our external address and enable hole-punching.
    swarm.dial(opts.relay_address.clone())?;
    println!("Connecting to relay server at {}", opts.relay_address);

    block_on(async {
        let mut learned_observed_addr = false;
        let mut told_relay_observed_addr = false;

        loop {
            match swarm.next().await.unwrap() {
                SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Sent { .. })) => {
                    println!("Told relay its public address.");
                    told_relay_observed_addr = true;
                }
                SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received {
                    info: identify::Info { observed_addr, .. },
                    ..
                })) => {
                    println!("Relay told us our observed address: {observed_addr}");
                    learned_observed_addr = true;
                }
                SwarmEvent::ConnectionEstablished { .. } => {}
                _ => {}
            }

            if learned_observed_addr && told_relay_observed_addr {
                break;
            }
        }
    });

    // Set up DCUtR connection depending on mode.
    match opts.mode {
        Mode::Dial => {
            let remote_peer_id = opts
                .remote_peer_id
                .expect("Dial mode requires a remote peer ID.");
            let relay_addr_with_circuit = opts
                .relay_address
                .with(Protocol::P2pCircuit)
                .with(Protocol::P2p(remote_peer_id));
            println!(
                "Dialing remote peer {} via relay circuit: {}",
                remote_peer_id, relay_addr_with_circuit
            );
            swarm.dial(relay_addr_with_circuit)?;
        }
        Mode::Listen => {
            let relay_addr_with_circuit = opts.relay_address.with(Protocol::P2pCircuit);
            println!(
                "Listening for connections via relay circuit: {}",
                relay_addr_with_circuit
            );
            swarm.listen_on(relay_addr_with_circuit)?;
        }
    }

    // Read lines from stdin to publish as gossipsub messages
    let mut stdin = io::BufReader::new(io::stdin()).lines();
    println!("\nEnter messages to send to peers. Press Ctrl+D to exit.");

    // Main event loop
    loop {
        select! {
            // Handle user input from stdin
            Ok(Some(line)) = stdin.next_line() => {
                if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), line.as_bytes()) {
                    println!("Publish error: {e:?}");
                }
            },
            // Handle swarm events
            event = swarm.select_next_some() => match event {
                SwarmEvent::NewListenAddr { address, .. } => {
                     println!("Listening on address: {address}");
                }
                SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                     println!("Established new connection with {peer_id} at {endpoint:?}");
                     // Explicitly add the new peer to gossipsub's mesh.
                     swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                }
                SwarmEvent::ConnectionClosed { peer_id, .. } => {
                    println!("Connection with {peer_id} closed.");
                    // Remove peer from gossipsub mesh.
                    swarm.behaviour_mut().gossipsub.remove_explicit_peer(&peer_id);
                }
                SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                     println!("Outgoing connection failed to {:?}: {error}", peer_id);
                }
                SwarmEvent::Behaviour(BehaviourEvent::RelayClient(
                    relay::client::Event::ReservationReqAccepted { .. },
                )) => {
                    assert!(opts.mode == Mode::Listen, "Should only receive reservation acceptance in listen mode.");
                    println!("Relay accepted our reservation request.");
                }
                SwarmEvent::Behaviour(BehaviourEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source: peer_id,
                    message_id: id,
                    message,
                })) => {
                     println!(
                        "Got message: '{}' with id: {id} from peer: {peer_id}",
                        String::from_utf8_lossy(&message.data),
                    );
                }
                SwarmEvent::Behaviour(event) => {
                    // Log other behaviours for debugging
                    tracing::info!(?event);
                }
                _ => {}
            }
        }
    }
}

/// Generates a deterministic Ed25519 keypair from a single byte seed.
fn generate_ed25519(secret_key_seed: u8) -> identity::Keypair {
    let mut bytes = [0u8; 32];
    bytes[0] = secret_key_seed;

    identity::Keypair::ed25519_from_bytes(bytes).expect("only errors on wrong length")
}