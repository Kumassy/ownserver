use std::{ops::RangeInclusive, sync::Arc};
use anyhow::Result;
use log::*;
use ownserver_lib::{EndpointClaim, Protocol};
use tokio_util::sync::CancellationToken;
use clap::Parser;

use ownserver::{api, proxy_client::{run_client, RequestType}, recorder::init_stdout_event_recorder, Config, Store};

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
    #[arg(long, default_value = "https://auth.ownserver.kumassy.com/v2/request_token", help = "Advanced settings")]
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
    init_stdout_event_recorder();

    let cli = Cli::parse();
    debug!("{:?}", cli);

    let config = Config {
        control_port: cli.control_port,
        token_server: cli.token_server,
        ping_interval: cli.periodic_ping_interval,
    };

    let store: Arc<Store> = Default::default();
    let cancellation_token = CancellationToken::new();


    let store_ = store.clone();

    if let Some(api_port) = cli.api_port {
        info!("client side api is available at localhost:{}", api_port);
        tokio::spawn(async move {
            api::spawn_api(store_, api_port).await;
        });
    }

    info!("start client main loop");
    run_client(&config, store, cancellation_token,
        RequestType::NewClient {
            endpoint_claims: cli.endpoint
        }
    ).await?;


    Ok(())
}

