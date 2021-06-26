pub use magic_tunnel_lib::{StreamId, ClientId, ClientHello, ServerHello, ControlPacket};
use futures::{SinkExt, StreamExt, Stream, Sink, AsyncRead, AsyncWrite, AsyncReadExt, AsyncWriteExt, channel::mpsc::unbounded};
// use log::*;
use std::net::{SocketAddr, IpAddr};
use tokio::task::JoinHandle;
use tracing::{info, Instrument};
use tracing_subscriber;
use warp::{Filter, Rejection, ws::{Ws, WebSocket, Message}, Error as WarpError};
use std::convert::Infallible;
use dashmap::DashMap;
use lazy_static::lazy_static;
use std::sync::Arc;

mod connected_clients;
mod active_stream;
mod control_server;
mod remote;
use crate::active_stream::ActiveStreams;
use crate::connected_clients::{ConnectedClient, Connections};
use crate::control_server::{spawn};

lazy_static! {
    pub static ref CONNECTIONS: Connections = Connections::new();
    pub static ref ACTIVE_STREAMS: ActiveStreams = Arc::new(DashMap::new());
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let addr: SocketAddr = ([0, 0, 0, 0], 5000).into();
    let handle = spawn(addr);
    info!("Listening on: {}", addr);

    handle.await.unwrap();
}