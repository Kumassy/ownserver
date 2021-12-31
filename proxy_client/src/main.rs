use anyhow::Result;
use log::*;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio_util::sync::CancellationToken;
use structopt::StructOpt;

use magic_tunnel_client::{proxy_client::run, ActiveStreams, error::Error};

lazy_static::lazy_static! {
    pub static ref ACTIVE_STREAMS: ActiveStreams = Arc::new(RwLock::new(HashMap::new()));
}

#[derive(StructOpt, Debug)]
#[structopt(name = "basic")]
struct Opt {
    #[structopt(long, default_value = "5000")]
    control_port: u16,

    #[structopt(long, default_value = "3000")]
    local_port: u16,

    #[structopt(long, default_value = "http://localhost:8123/v0/request_token")]
    token_server: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();
    let opt = Opt::from_args();
    debug!("{:?}", opt);

    let cancellation_token = CancellationToken::new();

    let (client_info, handle) =
        run(&ACTIVE_STREAMS, opt.control_port, opt.local_port, &opt.token_server, cancellation_token).await?;
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
