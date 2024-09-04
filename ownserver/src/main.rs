use std::{sync::Arc, ops::RangeInclusive};
use anyhow::Result;
use log::*;
use ownserver_lib::{EndpointClaim, Protocol};
use tokio_util::sync::CancellationToken;
use clap::Parser;

use ownserver::{proxy_client::run, api, Store};

#[derive(Parser, Debug)]
#[command(name = "ownserver")]
#[command(author, version, about, long_about = None)] 
struct Cli {
    #[arg(long, required = true, help = "Port and protocol of your local game server e.g.) `25565/tcp` for Minecraft", value_parser = parse_endpoint)]
    endpoint: Vec<EndpointClaim>,

    #[arg(long, help = "Advanced settings. You can inspect client's internal state at localhost:<api_port>.")]
    api_port: Option<u16>,
    #[arg(long, default_value_t = 5000, help = "Advanced settings")]
    control_port: u16,
    #[arg(long, default_value = "https://auth.ownserver.kumassy.com/v1/request_token", help = "Advanced settings")]
    token_server: String,

    #[structopt(long, default_value = "15")]
    periodic_ping_interval: u64,
}

const PORT_RANGE: RangeInclusive<usize> = 1..=65535;

fn parse_endpoint(s: &str) -> Result<EndpointClaim, String> {
    let mut parts = s.split('/');

    let port: usize = parts
        .next()
        .ok_or(format!("`{s}` isn't a valid endpoint"))?
        .parse()
        .map_err(|_| format!("`{s}` isn't a valid endpoint"))?;

    if !PORT_RANGE.contains(&port) {
        return Err(format!(
            "port not in range {}-{}",
            PORT_RANGE.start(),
            PORT_RANGE.end()
        ));
    } 
    let port = port as u16;

    let protocol = match parts.next() {
        Some("tcp") => Protocol::TCP,
        Some("udp") => Protocol::UDP,
        _ => return Err(format!("`{s}` isn't a valid protocol")),
    };

    Ok(EndpointClaim {
        protocol,
        local_port: port,
        remote_port: 0,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();

    let cli = Cli::parse();
    debug!("{:?}", cli);

    let store: Arc<Store> = Default::default();
    let cancellation_token = CancellationToken::new();


    let store_ = store.clone();
    let (client_info, mut set) =
        run(store_, cli.control_port, &cli.token_server, cancellation_token, cli.endpoint, cli.periodic_ping_interval).await?;
    info!("client is running under configuration: {:?}", client_info);

    if let Some(api_port) = cli.api_port {
        info!("client side api is available at localhost:{}", api_port);
        set.spawn(async move {
            api::spawn_api(store, api_port).await;
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
