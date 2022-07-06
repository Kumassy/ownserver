use std::{net::SocketAddr, sync::Arc};

use active_stream::{StreamMessage, ActiveStream};
use dashmap::DashMap;
use futures::{channel::mpsc::SendError, StreamExt, stream::{SplitSink, SplitStream}, SinkExt};
use magic_tunnel_lib::{ClientId, StreamId, ControlPacket};
use thiserror::Error;

pub mod active_stream;
pub mod connected_clients;
pub mod control_server;
pub mod remote;
pub mod proxy_server;
pub mod port_allocator;

#[derive(Debug, Clone)]
pub struct Config {
    pub control_port: u16,
    pub token_secret: String,
    pub host: String,
    pub remote_port_start: u16,
    pub remote_port_end: u16,
}


#[derive(Error, Debug, PartialEq)]
pub enum ProxyServerError {
    #[error("Failed to load config because it is not initialized.")]
    ConfigNotInitialized,
}

#[derive(Error, Debug, PartialEq)]
pub enum ForwardingError {
    #[error("Destination is disabled.")]
    DestinationDisabled,
    #[error("Failed to put data into sender buffer.")]
    SendError(#[from] SendError),
}

use tokio::{net::{TcpStream, UdpSocket}, io::{WriteHalf, AsyncReadExt, AsyncWriteExt}, select};
use tokio_util::sync::CancellationToken;
use warp::{
    ws::{Message, WebSocket, Ws},
    Error as WarpError, Filter,
};

#[derive(Debug)]
pub struct Client {
    pub id: ClientId,
    pub host: String,

    ws_tx: SplitSink<WebSocket, Message>,
    // ws_rx: SplitStream<WebSocket>,
    store: Arc<Store>,
    ct: CancellationToken,
}

impl Client {
    pub fn new(store: Arc<Store>, ws: WebSocket) -> Self {
        let (sink, mut stream) = ws.split();
        let token = CancellationToken::new();

        let ct = token.clone();

        tokio::spawn(async move {
            loop {
                select! {
                    _ = ct.cancelled() => {
                        return;
                    }
                    Some(packet) = stream.next() => {
                        let packet = packet;
                    }
                }
            }
        });

        Self { id: ClientId::new(), host: "hoo".into(), ws_tx: sink, store, ct: token }
    }

    pub async fn send_to_stream(&self, stream_id: StreamId, message: StreamMessage) -> Result<(), Box<dyn std::error::Error>> {
        self.store.streams.get_mut(&stream_id).unwrap().send_to_remote(stream_id, message).await?;
        Ok(())
    }

    pub async fn send_to_client(&mut self, packet: ControlPacket) -> Result<(), Box<dyn std::error::Error>> {
        self.ws_tx.send(Message::binary(b"foobar".to_vec())).await?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum RemoteStream {
    RemoteTcp(RemoteTcp),
    RemoteUdp,
}

// #[async_trait]
// trait RemoteStream {
//     async fn send_to_remote(&mut self, stream_id: StreamId, message: StreamMessage) -> Result<(), Box<dyn std::error::Error>>;
//     async fn send_to_client(&self, packet: ControlPacket) -> Result<(), Box<dyn std::error::Error>>;
// }


impl RemoteStream {
    pub async fn send_to_remote(&mut self, stream_id: StreamId, message: StreamMessage) -> Result<(), Box<dyn std::error::Error>> {
        match self {
            RemoteStream::RemoteTcp(tcp) => {
                tcp.send_to_remote(stream_id, message).await;
            }
            _ => {

            }
        }
        Ok(())
    }

    pub fn send_to_client(&self, packet: ControlPacket) -> Result<(), Box<dyn std::error::Error>> {

        Ok(())
    }
}

#[derive(Debug)]
pub struct RemoteTcp {
    pub id: StreamId,
    pub client_id: ClientId,
    socket_tx: WriteHalf<TcpStream>,
    store: Arc<Store>,
}

impl RemoteTcp {
    pub fn new(store: Arc<Store>, socket: TcpStream) -> Self {
        let (mut stream, sink) = tokio::io::split(socket);

        let mut buf = [0; 1024];
        
        tokio::spawn(async move {
            loop {
                let n = match stream.read(&mut buf).await {
                    Ok(n) => n,
                    Err(e) => {
                        return
                    }
                };
            }
        });

        Self { id: StreamId::new(), client_id: ClientId::new(), socket_tx: sink, store }
    }

    async fn send_to_remote(&mut self, stream_id: StreamId, message: StreamMessage) -> Result<(), Box<dyn std::error::Error>> {
        self.socket_tx.write_all(&b"hogehoge".to_vec()).await?;
        Ok(())
    }

    async fn send_to_client(&self, packet: ControlPacket) -> Result<(), Box<dyn std::error::Error>> {
        let client_id = self.client_id;
        self.store.clients.get_mut(&client_id).unwrap().send_to_client(packet).await?;
        Ok(())
    }
    
}

// #[async_trait]
// impl RemoteStream for RemoteTcp {
//     async fn send_to_remote(&mut self, stream_id: StreamId, message: StreamMessage) -> Result<(), Box<dyn std::error::Error>> {
//         self.socket_tx.write_all(&b"hogehoge".to_vec()).await?;
//         Ok(())
//     }

//     async fn send_to_client(&self, packet: ControlPacket) -> Result<(), Box<dyn std::error::Error>> {
//         Ok(())
//     }
// }


#[derive(Debug)]
pub struct RemoteUdp {
    pub id: StreamId,
    pub client_id: ClientId,
    socket: Arc<UdpSocket>,
    store: Arc<Store>,
}


#[derive(Debug)]
pub struct Store {
    // pub streams: DashMap<StreamId, Arc<dyn RemoteStream>>,
    pub streams: DashMap<StreamId, RemoteStream>,
    pub addrs_map: DashMap<SocketAddr, StreamId>,

    pub clients: DashMap<ClientId, Client>,
    pub hosts_map: DashMap<String, ClientId>,
}
