use dashmap::DashMap;
pub use magic_tunnel_lib::{ClientHello, ClientId, ControlPacket, ServerHello, StreamId};
use std::ops::Range;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use once_cell::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::active_stream::ActiveStreams;
use crate::connected_clients::Connections;
use crate::control_server;
use crate::port_allocator::PortAllocator;
use crate::{Config, ProxyServerError};

pub async fn run(
    config: &'static OnceCell<Config>,
    conn: &'static Connections,
    active_streams: &'static ActiveStreams,
    alloc: Arc<Mutex<PortAllocator<Range<u16>>>>,
    remote_cancellers: Arc<DashMap<ClientId, CancellationToken>>,
) -> Result<JoinHandle<()>, ProxyServerError> {
    tracing::info!("starting server!");

    let control_port = match config.get() {
        Some(config) => {
            config.control_port
        },
        None => {
            tracing::error!("failed to read config");
            return Err(ProxyServerError::ConfigNotInitialized);
        }
    };

    let handle = control_server::spawn(
        config,
        conn,
        active_streams,
        alloc,
        remote_cancellers,
        ([0, 0, 0, 0], control_port),
    );
    tracing::info!("started tunnelto server on 0.0.0.0:{}", control_port);
    Ok(handle)
}
