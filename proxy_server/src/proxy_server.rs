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
use tokio::sync::Mutex;
use std::ops::Range;

use crate::active_stream::ActiveStreams;
use crate::connected_clients::{ConnectedClient, Connections};
use crate::control_server;
use crate::remote::{self, CancelHander};
use crate::port_allocator::PortAllocator;


pub async fn run(
    conn: &'static Connections,
    active_streams: &'static ActiveStreams,
    alloc: Arc<Mutex<PortAllocator<Range<u16>>>>,
    remote_cancellers: Arc<DashMap<ClientId, CancelHander>>,
    control_port: u16)
{
    tracing::info!("starting server!");

    let handle = control_server::spawn(conn, active_streams, alloc, remote_cancellers, ([0, 0, 0, 0], control_port));
    info!("started tunnelto server on 0.0.0.0:{}", control_port);
    handle.await.unwrap(); // TODO: fix unwrap
}