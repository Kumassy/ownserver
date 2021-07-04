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

use crate::active_stream::ActiveStreams;
use crate::connected_clients::{ConnectedClient, Connections};
use crate::control_server;
use crate::remote;


pub async fn run(conn: &'static Connections, active_streams: &'static ActiveStreams, control_port: u16, remote_port: u16) {
    tracing::info!("starting server!");

    control_server::spawn(conn, active_streams, ([0, 0, 0, 0], control_port));
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
                remote::accept_connection(conn, active_streams.clone(), socket, "host-foobar".to_string()).await;
            }
            .instrument(tracing::info_span!("remote_connect")),
        );
    }
}