use ownserver_server::Store;
pub use ownserver_server::{
    port_allocator::PortAllocator,
    proxy_server::run,
    Config,
};
use metrics::{describe_counter, describe_gauge};
use metrics_exporter_prometheus::PrometheusBuilder;
use tracing_subscriber::prelude::*;
use std::{sync::Arc, fs};
use tokio::sync::Mutex;
use once_cell::sync::OnceCell;
use structopt::StructOpt;
use opentelemetry::{sdk::{trace::{self, XrayIdGenerator}, Resource}, KeyValue};

static CONFIG: OnceCell<Config> = OnceCell::new();

#[derive(StructOpt, Debug)]
#[structopt(name = "ownserver-server")]
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

    #[structopt(long, default_value = "/var/log/ownserver/proxy_server.log")]
    log_file: String,
}

impl From<Opt> for Config {
    fn from(opt: Opt) -> Config {
        let Opt {
            control_port,
            token_secret,
            host,
            remote_port_start,
            remote_port_end,
            ..
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
    // for tokio-console
    // console_subscriber::init();

    let opt = Opt::from_args();
    let log_file = opt.log_file.clone();
    let config = Config::from(opt);
    CONFIG.set(config).expect("failed to initialize config");

    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(&log_file)
        .unwrap_or_else(|_| panic!("failed to open log file {}", log_file));

    let tracer = opentelemetry_jaeger::new_pipeline()
        .with_service_name("ownserver")
        .with_trace_config(
            trace::config()
                .with_id_generator(XrayIdGenerator::default())
                .with_resource(Resource::new(vec![
                    KeyValue {
                        key: "hostname".into(),
                        value: CONFIG.get().expect("failed to read config").host.clone().into()
                    }
                ]))
        )
        .install_batch(opentelemetry::runtime::Tokio)
        .expect("Failed to initialize tracer");
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("INFO"))
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(file))
        .try_init()
        .expect("Failed to register tracer with registry");

    let builder = PrometheusBuilder::new();
    builder.install().expect("failed to install recorder/exporter");
    describe_gauge!("ownserver_server.store.clients", "[gauge] The number of Clients at this time.");
    describe_gauge!("ownserver_server.store.streams", "[gauge] The number of RemoteStreams at this time.");
    describe_counter!("ownserver_server.control_server.handle_new_connection", "[counter] The number of successfully accepted websocket connections so far.");
    describe_counter!("ownserver_server.control_server.try_client_handshake.success", "[counter] The number of succesfully handshake requests so far.");
    describe_counter!("ownserver_server.control_server.try_client_handshake.service_temporary_unavailable", "[counter] The number of handshake error ServiceTemporaryUnavailable so far.");
    describe_counter!("ownserver_server.control_server.try_client_handshake.invalid_client_hello", "[counter] The number of handshake error InvalidClientHello so far.");
    describe_counter!("ownserver_server.control_server.try_client_handshake.invalid_jwt", "[counter] The number of handshake error InvalidJWT so far.");
    describe_counter!("ownserver_server.control_server.try_client_handshake.illegal_host", "[counter] The number of handshake error IllegalHost so far.");
    describe_counter!("ownserver_server.control_server.try_client_handshake.version_mismatch", "[counter] The number of handshake error VersionMismatch so far.");
    describe_counter!("ownserver_server.control_server.try_client_handshake.other", "[counter] The number of handshake error Other so far.");
    describe_counter!("ownserver_server.remote.tcp.swawn_remote", "[counter] How many times tcp::spawn_remote called.");
    describe_counter!("ownserver_server.remote.udp.swawn_remote", "[counter] How many times udp::spawn_remote called.");
    tracing::info!("Prometheus endpoint: localhost:9000");

    tracing::debug!("{:?}", CONFIG.get().expect("failed to read config"));
    let Config {remote_port_start, remote_port_end  , ..}  = CONFIG.get().expect("failed to read config");

    let store = Arc::new(Store::new(*remote_port_start..*remote_port_end));

    let handle = run(
        &CONFIG,
        store,
    ).await;
    
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
