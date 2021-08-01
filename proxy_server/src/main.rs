use std::sync::Arc;
use lazy_static::lazy_static;
use dashmap::DashMap;
use tokio::sync::Mutex;
pub use magic_tunnel_server::{
    proxy_server::run,
    active_stream::ActiveStreams,
    connected_clients::Connections,
    port_allocator::PortAllocator,
    remote::CancelHander,
};
use magic_tunnel_lib::ClientId;

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
    run(&CONNECTIONS, &ACTIVE_STREAMS, alloc, remote_cancellers, control_port).await;
}