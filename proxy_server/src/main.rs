use dashmap::DashMap;
use lazy_static::lazy_static;
use magic_tunnel_lib::ClientId;
pub use magic_tunnel_server::{
    active_stream::ActiveStreams, connected_clients::Connections, port_allocator::PortAllocator,
    proxy_server::run,
    Config,
};
use tracing_subscriber::prelude::*;
use std::sync::Arc;
use tokio::sync::Mutex;
use once_cell::sync::OnceCell;
use tokio_util::sync::CancellationToken;
use structopt::StructOpt;
use opentelemetry::sdk::trace::{self, XrayIdGenerator};

lazy_static! {
    pub static ref CONNECTIONS: Connections = Connections::new();
    pub static ref ACTIVE_STREAMS: ActiveStreams = Arc::new(DashMap::new());
}

static CONFIG: OnceCell<Config> = OnceCell::new();

#[derive(StructOpt, Debug)]
#[structopt(name = "basic")]
struct Opt {
    #[structopt(long, default_value = "5000")]
    control_port: u16,

    #[structopt(long, env = "MT_TOKEN_SECRET")]
    token_secret: String,

    #[structopt(short, long)]
    host: String,

    #[structopt(long)]
    remote_port_start: u16,

    #[structopt(long)]
    remote_port_end: u16,
}

impl From<Opt> for Config {
    fn from(opt: Opt) -> Config {
        let Opt {
            control_port,
            token_secret,
            host,
            remote_port_start,
            remote_port_end,
        } = opt;

        Config {
            control_port,
            token_secret,
            host,
            remote_port_start,
            remote_port_end,
        }
    }
}

#[tokio::main]
async fn main() {
    let tracer = opentelemetry_jaeger::new_pipeline()
        .with_collector_endpoint("http://localhost:14268/api/traces")
        .with_service_name("magic-tunnel-server")
        .with_trace_config(
            trace::config().with_id_generator(XrayIdGenerator::default())
        )
        .install_simple()
        .expect("Failed to initialize tracer");
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("DEBUG"))
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        .try_init()
        .expect("Failed to register tracer with registry");

    let opt = Opt::from_args();
    tracing::debug!("{:?}", opt);
    let config = Config::from(opt);

    if CONFIG.set(config).is_err() {
        tracing::error!("failed to initialize config");
        return;
    }

    let (remote_port_start, remote_port_end) = match CONFIG.get() {
        Some(config) => {
            (config.remote_port_start, config.remote_port_end)
        },
        None => {
            tracing::error!("failed to read config");
            return;
        }
    };

    let alloc = Arc::new(Mutex::new(PortAllocator::new(remote_port_start..remote_port_end)));
    let remote_cancellers: Arc<DashMap<ClientId, CancellationToken>> = Arc::new(DashMap::new());
    let handle = run(
        &CONFIG,
        &CONNECTIONS,
        &ACTIVE_STREAMS,
        alloc,
        remote_cancellers,
    )
    .await;
    
    let handle = match handle {
        Ok(handle) => handle,
        Err(_) => {
            tracing::error!("failed to read config");
            return;
        }
    };
    
    let server = handle.await;
    match server {
        Err(join_error) => {
            tracing::error!("join error {:?} for proxy_server", join_error);
        }
        Ok(_) => {
            tracing::info!("proxy_server successfully terminated");
        }
    }

    opentelemetry::global::shutdown_tracer_provider();
}
