pub use magic_tunnel_lib::{StreamId, ClientId, ClientHello, ServerHello, ControlPacket};
use futures::{SinkExt, StreamExt, Stream, Sink, AsyncRead, AsyncWrite, AsyncReadExt, AsyncWriteExt, channel::mpsc::unbounded};
// use log::*;
use std::net::{SocketAddr, IpAddr};
use tokio::task::JoinHandle;
use tracing_subscriber;
use warp::{Filter, Rejection, ws::{Ws, WebSocket, Message}, Error as WarpError};
use std::convert::Infallible;
use dashmap::DashMap;
use lazy_static::lazy_static;
use std::sync::Arc;
use tracing::{error, info, Instrument};
use tokio::net::TcpListener;

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
    let control_port: u16 = 5000;
    let remote_port: u16 = 8080;
    tracing_subscriber::fmt::init();


    tracing::info!("starting server!");

    control_server::spawn(([0, 0, 0, 0], control_port));
    info!("started tunnelto server on 0.0.0.0:{}", control_port);

    let listen_addr = format!("[::]:{}", remote_port);
    info!("listening on: {}", &listen_addr);

    // create our accept any server
    let listener = TcpListener::bind(listen_addr)
        .await
        .expect("failed to bind");

    loop {
        let socket = match listener.accept().await {
            Ok((socket, _)) => socket,
            _ => {
                error!("failed to accept socket");
                continue;
            }
        };

        tokio::spawn(
            async move {
                remote::accept_connection(&CONNECTIONS, ACTIVE_STREAMS.clone(), socket, "host-foobar".to_string()).await;
            }
            .instrument(tracing::info_span!("remote_connect")),
        );
    }
}