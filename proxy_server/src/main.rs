use std::sync::Arc;
use lazy_static::lazy_static;
use dashmap::DashMap;
pub use magic_tunnel_server::{proxy_server::run, active_stream::ActiveStreams, connected_clients::Connections};

lazy_static! {
    pub static ref CONNECTIONS: Connections = Connections::new();
    pub static ref ACTIVE_STREAMS: ActiveStreams = Arc::new(DashMap::new());
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let control_port: u16 = 5000;
    let remote_port: u16 = 8080;

    run(&CONNECTIONS, &ACTIVE_STREAMS, control_port, remote_port).await;
}