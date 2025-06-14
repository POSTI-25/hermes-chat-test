mod dcutr;
mod chat;

#[tokio::main]
async fn main() {
    if let Err(e) = dcutr::start().await {
        eprintln!("Error in DCUtR: {e}");
        return;
    }

    if let Err(e) = chat::run_chat().await {
        eprintln!("Error in chat: {e}");
    }
}
