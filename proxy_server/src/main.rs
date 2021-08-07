use dashmap::DashMap;
use lazy_static::lazy_static;
use magic_tunnel_lib::ClientId;
pub use magic_tunnel_server::{
    active_stream::ActiveStreams, connected_clients::Connections, port_allocator::PortAllocator,
    proxy_server::run, remote::CancelHander,
};
use std::sync::Arc;
use tokio::sync::Mutex;

lazy_static! {
    pub static ref CONNECTIONS: Connections = Connections::new();
    pub static ref ACTIVE_STREAMS: ActiveStreams = Arc::new(DashMap::new());
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let control_port: u16 = 5000;

    let alloc = Arc::new(Mutex::new(PortAllocator::new(3000..4000)));
    let remote_cancellers: Arc<DashMap<ClientId, CancelHander>> = Arc::new(DashMap::new());
    let handle = run(
        &CONNECTIONS,
        &ACTIVE_STREAMS,
        alloc,
        remote_cancellers,
        control_port,
    )
    .await;
    
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
