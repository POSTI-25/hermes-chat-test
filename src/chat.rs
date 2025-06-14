use futures::prelude::*;
use libp2p::{
    request_response::{
        ProtocolSupport, RequestResponse, RequestResponseCodec,
        RequestResponseConfig, RequestResponseEvent, RequestResponseMessage,
    },
    swarm::{Swarm, SwarmEvent},
    PeerId,
};
use std::{
    error::Error,
    io::{self, BufReader},
    str,
    task::{Context, Poll},
    time::Duration,
};
use async_trait::async_trait;
use tokio::io::AsyncBufReadExt;

// === Custom Protocol Definition ===

#[derive(Debug, Clone)]
struct ChatProtocol();

#[derive(Clone)]
struct ChatCodec();

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChatRequest(String);

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChatResponse(String);

#[async_trait]
impl RequestResponseCodec for ChatCodec {
    type Protocol = ChatProtocol;
    type Request = ChatRequest;
    type Response = ChatResponse;

    async fn read_request<T>(&mut self, _: &ChatProtocol, io: &mut T) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        let mut buf = Vec::new();
        io.read_to_end(&mut buf).await?;
        Ok(ChatRequest(String::from_utf8_lossy(&buf).to_string()))
    }

    async fn read_response<T>(&mut self, _: &ChatProtocol, io: &mut T) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        let mut buf = Vec::new();
        io.read_to_end(&mut buf).await?;
        Ok(ChatResponse(String::from_utf8_lossy(&buf).to_string()))
    }

    async fn write_request<T>(
        &mut self,
        _: &ChatProtocol,
        io: &mut T,
        ChatRequest(data): ChatRequest,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        io.write_all(data.as_bytes()).await?;
        io.close().await
    }

    async fn write_response<T>(
        &mut self,
        _: &ChatProtocol,
        io: &mut T,
        ChatResponse(data): ChatResponse,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        io.write_all(data.as_bytes()).await?;
        io.close().await
    }
}

// === Add the chat protocol to an existing Swarm ===

pub fn inject_chat_behaviour(
    swarm: &mut Swarm<crate::dcutr::Behaviour>,
) -> &mut RequestResponse<ChatCodec> {
    let chat_proto = ChatProtocol();
    let protocols = std::iter::once((
        chat_proto.clone(),
        ProtocolSupport::Full,
    ));

    let cfg = RequestResponseConfig::default();
    let chat = RequestResponse::new(ChatCodec(), protocols, cfg);
    swarm.behaviour_mut().insert_protocol(chat)
}

// === Start Chatting ===

pub async fn start_chat(
    mut swarm: Swarm<crate::dcutr::Behaviour>,
    local_peer_id: PeerId,
) -> Result<(), Box<dyn Error>> {
    println!("Chat ready! Your PeerId is: {local_peer_id}");
    println!("Type your message and press Enter to send:");

    // Add chat protocol dynamically to the existing swarm
    let chat = inject_chat_behaviour(&mut swarm);
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();

    tokio::spawn(async move {
        while let Some(Ok(line)) = lines.next_line().await {
            if line.is_empty() {
                continue;
            }

            // Send to all connected peers
            for peer in chat.connected_peers() {
                chat.send_request(peer, ChatRequest(line.clone()));
            }
        }
    });

    loop {
        match swarm.select_next_some().await {
            SwarmEvent::Behaviour(crate::dcutr::BehaviourEvent::Custom(event)) => {
                match event {
                    RequestResponseEvent::Message { peer, message } => {
                        match message {
                            RequestResponseMessage::Request {
                                request, channel, ..
                            } => {
                                println!("\n<< {} says: {}", peer, request.0);
                                chat.send_response(channel, ChatResponse("ok".into())).unwrap();
                            }
                            RequestResponseMessage::Response { response, .. } => {
                                println!("\n>> Received ack: {}", response.0);
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}
