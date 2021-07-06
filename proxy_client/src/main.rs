use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use magic_tunnel_client::{ActiveStreams, proxy_client::{run}};

lazy_static::lazy_static! {
    pub static ref ACTIVE_STREAMS: ActiveStreams = Arc::new(RwLock::new(HashMap::new()));
}

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();
    let control_port: u16 = 5000;
    let remote_port: u16 = 8080;
    let local_port: u16 = 3000;

    run(&ACTIVE_STREAMS, control_port, remote_port, local_port).await?;

    Ok(())
}