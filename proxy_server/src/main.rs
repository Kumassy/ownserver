use dashmap::DashMap;
use lazy_static::lazy_static;
use magic_tunnel_lib::ClientId;
pub use magic_tunnel_server::{
    active_stream::ActiveStreams, connected_clients::Connections, port_allocator::PortAllocator,
    proxy_server::run,
    Config,
};
use std::sync::Arc;
use tokio::sync::Mutex;
use once_cell::sync::OnceCell;
use tokio_util::sync::CancellationToken;

lazy_static! {
    pub static ref CONNECTIONS: Connections = Connections::new();
    pub static ref ACTIVE_STREAMS: ActiveStreams = Arc::new(DashMap::new());
}

static CONFIG: OnceCell<Config> = OnceCell::new();

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config = Config {
        control_port: 5000,
        token_secret: "supersecret".to_string(),
        host: "foohost.local".to_string(),
        remote_port_start: 3000,
        remote_port_end: 4000,
    };
    if CONFIG.set(config).is_err() {
        tracing::error!("failed to initialize config");
        return;
    }

    let (remote_port_start, remote_port_end) = match CONFIG.get() {
        Some(config) => {
            (config.remote_port_start, config.remote_port_end)
        },
        None => {
            tracing::error!("failed to read config");
            return;
        }
    };

    let alloc = Arc::new(Mutex::new(PortAllocator::new(remote_port_start..remote_port_end)));
    let remote_cancellers: Arc<DashMap<ClientId, CancellationToken>> = Arc::new(DashMap::new());
    let handle = run(
        &CONFIG,
        &CONNECTIONS,
        &ACTIVE_STREAMS,
        alloc,
        remote_cancellers,
    )
    .await;
    
    let handle = match handle {
        Ok(handle) => handle,
        Err(_) => {
            tracing::error!("failed to read config");
            return;
        }
    };
    
    let server = handle.await;
    match server {
        Err(join_error) => {
            tracing::error!("join error {:?} for proxy_server", join_error);
        }
        Ok(_) => {
            tracing::info!("proxy_server successfully terminated");
        }
    }
}
