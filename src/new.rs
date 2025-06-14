use std::{error::Error, str::FromStr, time::Duration};

use clap::Parser;
use futures::{prelude::*, stream::StreamExt};
use libp2p::{
    core::multiaddr::{Multiaddr, Protocol},
    dcutr, gossipsub::{self, GossipsubEvent, IdentTopic, MessageAuthenticity, ValidationMode},
    identify, identity, noise, ping, relay,
    swarm::{NetworkBehaviour,SwarmBuilder, SwarmEvent},
    tcp, yamux, PeerId,
};
use tokio::io::{self, AsyncBufReadExt};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "libp2p DCUtR + Chat")]
struct Opts {
    /// Mode: dial or listen
    #[arg(long)]
    mode: Mode,

    /// Secret key seed
    #[arg(long)]
    secret_key_seed: u8,

    /// Relay server address
    #[arg(long)]
    relay_address: Multiaddr,

    /// Remote peer ID (required in dial mode)
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
            _ => Err("Expected either 'dial' or 'listen'".into()),
        }
    }
}

#[derive(NetworkBehaviour)]
#[behaviour(to_swarm = "MyBehaviourEvent")]
struct MyBehaviour {
    relay_client: relay::client::Behaviour,
    ping: ping::Behaviour,
    identify: identify::Behaviour,
    dcutr: dcutr::Behaviour,
    gossipsub: gossipsub::Behaviour,
}

#[allow(clippy::large_enum_variant)]
enum MyBehaviourEvent {
    RelayClient(relay::client::Event),
    Ping(ping::Event),
    Identify(identify::Event),
    Dcutr(dcutr::Event),
    Gossipsub(gossipsub::Event),
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init()
        .ok();

    let opts = Opts::parse();

    let id_keys = generate_ed25519(opts.secret_key_seed);
    let peer_id = id_keys.public().to_peer_id();
    println!("Local Peer ID: {peer_id}");

    let gossipsub_config = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_secs(10))
        .validation_mode(ValidationMode::Strict)
        .build()
        .unwrap();

    let gossipsub = gossipsub::Behaviour::new(
        MessageAuthenticity::Signed(id_keys.clone()),
        gossipsub_config,
    )?;

    let behaviour = |keypair: identity::Keypair, relay_client| MyBehaviour {
        relay_client,
        ping: ping::Behaviour::new(ping::Config::new()),
        identify: identify::Behaviour::new(identify::Config::new(
            "/chat/0.0.1".into(),
            keypair.public(),
        )),
        dcutr: dcutr::Behaviour::new(keypair.public().to_peer_id()),
        gossipsub,
    };

    let mut swarm = SwarmBuilder::with_existing_identity(id_keys.clone())
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_dns()?
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(behaviour)?
        .build();

    // Listen
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;
    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()?)?;

    // Wait for listener
    {
        let mut delay = futures_timer::Delay::new(Duration::from_secs(1)).fuse();
        loop {
            futures::select! {
                event = swarm.next() => {
                    if let SwarmEvent::NewListenAddr { address, .. } = event.unwrap() {
                        println!("Listening on {address}");
                    }
                }
                _ = delay => break,
            }
        }
    }

    // Connect to relay
    swarm.dial(opts.relay_address.clone())?;
    {
        let mut learned_observed = false;
        let mut told_observed = false;

        loop {
            match swarm.next().await.unwrap() {
                SwarmEvent::Behaviour(MyBehaviourEvent::Identify(identify::Event::Received {
                    info: identify::Info { observed_addr, .. },
                    ..
                })) => {
                    println!("Relay told us our public addr: {observed_addr}");
                    learned_observed = true;
                }
                SwarmEvent::Behaviour(MyBehaviourEvent::Identify(identify::Event::Sent { .. })) => {
                    println!("We told relay our observed address");
                    told_observed = true;
                }
                _ => {}
            }

            if learned_observed && told_observed {
                break;
            }
        }
    }

    // Set up relayed connection or listen
    match opts.mode {
        Mode::Dial => {
            let remote = opts.remote_peer_id.unwrap();
            let address = opts
                .relay_address
                .clone()
                .with(Protocol::P2pCircuit)
                .with(Protocol::P2p(remote));
            println!("Dialing peer at: {address}");
            swarm.dial(address)?;
        }
        Mode::Listen => {
            swarm
                .listen_on(opts.relay_address.clone().with(Protocol::P2pCircuit))?;
        }
    }

    // Subscribe to topic
    let topic = IdentTopic::new("p2p-chat");
    swarm.behaviour_mut().gossipsub.subscribe(&topic)?;

    println!("You can now type messages:");

    // Async stdin
    let mut stdin = io::BufReader::new(io::stdin()).lines();

    // Main loop
    loop {
        tokio::select! {
            line = stdin.next_line() => {
                if let Ok(Some(text)) = line {
                    let msg = text.as_bytes();
                    if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), msg) {
                        eprintln!("Publish error: {e}");
                    }
                }
            }
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(GossipsubEvent::Message {
                        propagation_source: peer_id,
                        message_id: _,
                        message,
                    })) => {
                        println!("Peer {peer_id}: {}", String::from_utf8_lossy(&message.data));
                    }
                    SwarmEvent::Behaviour(MyBehaviourEvent::RelayClient(
                        relay::client::Event::ReservationReqAccepted { .. },
                    )) => {
                        println!("Relay accepted our reservation");
                    }
                    SwarmEvent::Behaviour(MyBehaviourEvent::Dcutr(ev)) => {
                        println!("DCUtR: {ev:?}");
                    }
                    SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                        println!("Connected to {peer_id}");
                    }
                    SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                        println!("Connection error to {peer_id:?}: {error}");
                    }
                    _ => {}
                }
            }
        }
    }
}

fn generate_ed25519(secret_key_seed: u8) -> identity::Keypair {
    let mut bytes = [0u8; 32];
    bytes[0] = secret_key_seed;
    identity::Keypair::ed25519_from_bytes(bytes).expect("32 byte array expected")
}
