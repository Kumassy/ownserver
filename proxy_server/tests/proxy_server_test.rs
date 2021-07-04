// integrated server test
use std::sync::Arc;
use dashmap::DashMap;
use lazy_static::lazy_static;
pub use magic_tunnel_server::{proxy_server::run, active_stream::ActiveStreams, connected_clients::Connections};

#[cfg(test)]
mod tunnel_to_stream_test {
    use super::*;

    #[tokio::test]
    async fn mock_test() -> Result<(), Box<dyn std::error::Error>> {
        lazy_static! {
            pub static ref CONNECTIONS: Connections = Connections::new();
            pub static ref ACTIVE_STREAMS: ActiveStreams = Arc::new(DashMap::new());
        }

        let control_port: u16 = 5000;
        let remote_port: u16 = 8080;

        // run(&CONNECTIONS, &ACTIVE_STREAMS, control_port, remote_port).await;
        Ok(())
    }
}