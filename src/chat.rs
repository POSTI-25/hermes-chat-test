use std::{
    collections::hash_map::DefaultHasher,
    error::Error,
    hash::{Hash, Hasher},
    time::Duration,
};

use futures::stream::StreamExt;
use libp2p::{
    gossipsub, noise,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId,
};
use tokio::{io, io::AsyncBufReadExt, select};
use tracing_subscriber::EnvFilter;

// Custom behaviour combining Gossipsub, Relay, and DCUtR
#[derive(NetworkBehaviour)]
struct MyBehaviour {
    gossipsub: gossipsub::Behaviour,
    relay: libp2p::relay::Behaviour,
    dcutr: libp2p::dcutr::Behaviour,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Logging setup
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    // Swarm setup
    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_behaviour(|key| {
            // Gossipsub message ID hash function
            let message_id_fn = |message: &gossipsub::Message| {
                let mut s = DefaultHasher::new();
                message.data.hash(&mut s);
                gossipsub::MessageId::from(s.finish().to_string())
            };

            // Gossipsub config
            let gossipsub_config = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(10))
                .validation_mode(gossipsub::ValidationMode::Strict)
                .message_id_fn(message_id_fn)
                .build()
                .map_err(io::Error::other)?;

            // Gossipsub behaviour
            let gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
            )?;

            // Relay and DCUtR behaviours
            let relay = libp2p::relay::Behaviour::new(key.public().to_peer_id());
            let dcutr = libp2p::dcutr::Behaviour::new(key.public().to_peer_id());

            Ok(MyBehaviour { gossipsub, relay, dcutr })
        })?
        .build();

    // Listen on all interfaces and dynamic ports
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;
    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()?)?;

    // Subscribe to Gossipsub topic
    let topic = gossipsub::IdentTopic::new("test-net");
    swarm.behaviour_mut().gossipsub.subscribe(&topic)?;

    // Replace with actual relay + remote peer address
    let remote_addr: Multiaddr = "/ip4/RELAY_IP/tcp/4001/p2p/RELAY_PEER_ID/p2p-circuit/p2p/REMOTE_PEER_ID".parse()?;
    swarm.dial(remote_addr.clone())?;
    println!("Dialing remote peer via relay at: {remote_addr}");

    // Read from STDIN
    let mut stdin = io::BufReader::new(io::stdin()).lines();

    println!("Enter messages to send via Gossipsub:");

    // Main loop
    loop {
        select! {
            Ok(Some(line)) = stdin.next_line() => {
                if let Err(e) = swarm
                    .behaviour_mut().gossipsub
                    .publish(topic.clone(), line.as_bytes()) {
                    println!("Publish error: {e:?}");
                }
            }
            event = swarm.select_next_some() => match event {
                SwarmEvent::Behaviour(MyBehaviourEvent::Dcutr(event)) => {
                    println!("DCUtR event: {:?}", event);
                }
                SwarmEvent::Behaviour(MyBehaviourEvent::Relay(event)) => {
                    println!("Relay event: {:?}", event);
                }
                SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source: peer_id,
                    message_id,
                    message,
                })) => {
                    println!(
                        "Received from {peer_id}: '{}' (id: {message_id})",
                        String::from_utf8_lossy(&message.data),
                    );
                }
                SwarmEvent::NewListenAddr { address, .. } => {
                    println!("Listening on: {address}");
                }
                _ => {}
            }
        }
    }
}


// 
// Replace RELAY_IP, RELAY_PEER_ID, and REMOTE_PEER_ID with real values.

// You can get these by running your relay node and logging the peer ID + address.
// 