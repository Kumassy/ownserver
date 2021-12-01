use anyhow::Result;
use log::*;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;

use magic_tunnel_client::{proxy_client::run, ActiveStreams, error::Error};

lazy_static::lazy_static! {
    pub static ref ACTIVE_STREAMS: ActiveStreams = Arc::new(RwLock::new(HashMap::new()));
}

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();
    let control_port: u16 = 5000;
    let local_port: u16 = 3000;
    let token_server = "http://localhost:8123/v0/request_token";
    let cancellation_token = CancellationToken::new();

    let (client_info, handle) =
        run(&ACTIVE_STREAMS, control_port, local_port, token_server, cancellation_token).await?;
    info!("client is running under configuration: {:?}", client_info);


    match handle.await {
        Err(e) => {
            error!("join error {:?} for client", e);
        }
        Ok(Err(Error::JoinError(e))) => {
            error!("internal join error {:?} for client", e);
        }
        Ok(Err(e)) => {
            error!("client exited. reason: {:?}", e);
        }
        Ok(Ok(_)) => {
            info!("client successfully terminated");
        }
    }

    Ok(())
}
