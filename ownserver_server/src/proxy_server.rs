pub use ownserver_lib::{ClientHello, ClientId, ControlPacket, ServerHello, StreamId};
use std::ops::Range;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use once_cell::sync::OnceCell;

use crate::{control_server, Store};
use crate::port_allocator::PortAllocator;
use crate::{Config, ProxyServerError};

#[tracing::instrument(skip(config, store, alloc))]
pub async fn run(
    config: &'static OnceCell<Config>,
    store: Arc<Store>,
    alloc: Arc<Mutex<PortAllocator<Range<u16>>>>,
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
        store,
        alloc,
        ([0, 0, 0, 0], control_port));
    tracing::info!("started tunnelto server on 0.0.0.0:{}", control_port);
    Ok(handle)
}
