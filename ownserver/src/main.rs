use std::sync::Arc;
use anyhow::Result;
use log::*;
use ownserver_lib::{Payload, Endpoint, EndpointClaims, EndpointClaim, Protocol};
use tokio_util::sync::CancellationToken;
use structopt::StructOpt;

use ownserver::{proxy_client::run, api, Store};

#[derive(StructOpt, Debug)]
#[structopt(name = "ownserver")]
struct Opt {
    #[structopt(long, default_value = "3000", help = "Port of your local game server listens e.g.) 25565 for Minecraft")]
    local_port: u16,
    #[structopt(long, default_value = "tcp", help = "tcp or udp")]
    payload: String,
    #[structopt(long, help = "Advanced settings. You can inspect client's internal state at localhost:<api_port>.")]
    api_port: Option<u16>,

    #[structopt(long, default_value = "5000", help = "Advanced settings")]
    control_port: u16,
    #[structopt(long, default_value = "https://auth.ownserver.kumassy.com/v1/request_token", help = "Advanced settings")]
    token_server: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();
    let opt = Opt::from_args();
    debug!("{:?}", opt);

    let protocol = match opt.payload.as_str() {
        "udp" => Protocol::UDP,
        _ => Protocol::TCP,
    };
    debug!("{:?}", protocol);

    let store: Arc<Store> = Default::default();
    let cancellation_token = CancellationToken::new();

    let endpoint_claims = vec![EndpointClaim {
        protocol,
        local_port: opt.local_port,
        remote_port: 0,
    }];
        
    let store_ = store.clone();
    let (client_info, mut set) =
        run(store_, opt.control_port, &opt.token_server, cancellation_token, endpoint_claims).await?;
    info!("client is running under configuration: {:?}", client_info);

    if let Some(api_port) = opt.api_port {
        info!("client side api is available at localhost:{}", api_port);
        set.spawn(async move {
            api::spawn_api(store, api_port, opt.local_port, client_info).await;
            Ok(())
        });
    }

    while let Some(res) = set.join_next().await {
        match res {
            Err(join_error) => {
                error!("join error {:?} for client", join_error);
            }
            Ok(_) => {
                info!("client successfully terminated");
            }
        }
    }

    Ok(())
}
