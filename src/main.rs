pub mod dcutr;

use dcutr::{run, Opts};
// use chat::start_chat;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let opts = Opts::parse();
    let (swarm, peer_id) = run(opts).await?;
    
    // lets opts
    // Call chat functionality here
    // start_chat(swarm, peer_id).await?;
    
    Ok(())
}

// swarm event , peer id 