pub use magic_tunnel_lib::{StreamId, ClientId, ClientHello, ServerHello, ControlPacket};
use log::*;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use std::ops::Range;

use crate::active_stream::ActiveStreams;
use crate::connected_clients::Connections;
use crate::control_server;
use crate::remote::CancelHander;
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