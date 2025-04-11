use chrono::Duration;
use metrics_util::MetricKindMask;
use ownserver_server::Store;
pub use ownserver_server::{
    port_allocator::PortAllocator,
    proxy_server::run,
    Config,
};
use metrics::{describe_counter, describe_gauge};
use metrics_exporter_prometheus::PrometheusBuilder;
use tracing_subscriber::prelude::*;
use std::sync::Arc;
use std::sync::OnceLock;
use structopt::StructOpt;

static CONFIG: OnceLock<Config> = OnceLock::new();

#[derive(StructOpt, Debug)]
#[structopt(name = "ownserver-server")]
struct Opt {
    #[structopt(long, default_value = "5000")]
    control_port: u16,

    #[structopt(long, env = "MT_TOKEN_SECRET")]
    token_secret: String,

    #[structopt(long, default_value = "300")]
    metrics_idle_timeout: u64,

    #[structopt(short, long)]
    host: String,

    #[structopt(long)]
    remote_port_start: u16,

    #[structopt(long)]
    remote_port_end: u16,

    #[structopt(long, default_value = "15")]
    periodic_cleanup_interval: u64,

    #[structopt(long, default_value = "15")]
    periodic_ping_interval: u64,

    #[structopt(long, default_value = "2")]
    reconnect_window_minutes: i64,
}

impl From<Opt> for Config {
    fn from(opt: Opt) -> Config {
        let Opt {
            control_port,
            token_secret,
            host,
            metrics_idle_timeout,
            remote_port_start,
            remote_port_end,
            periodic_cleanup_interval,
            periodic_ping_interval,
            reconnect_window_minutes,
            ..
        } = opt;

        Config {
            control_port,
            token_secret,
            host,
            metrics_idle_timeout,
            remote_port_start,
            remote_port_end,
            periodic_cleanup_interval,
            periodic_ping_interval,
            reconnect_window: Duration::minutes(reconnect_window_minutes),
        }
    }
}

#[tokio::main]
async fn main() {
    // for tokio-console
    // console_subscriber::init();

    let opt = Opt::from_args();
    let config = Config::from(opt);
    CONFIG.set(config).expect("failed to initialize config");

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("INFO"))
        .with(tracing_subscriber::fmt::layer())
        .try_init()
        .expect("Failed to register tracer with registry");


    let metrics_idle_timeout = CONFIG.get().expect("failed to read config").metrics_idle_timeout;
    let builder = PrometheusBuilder::new();
    builder
        .idle_timeout(MetricKindMask::ALL, Some(std::time::Duration::from_secs(metrics_idle_timeout)))
        .install()
        .expect("failed to install recorder/exporter");

    describe_counter!("ownserver_server.remote.tcp.swawn_remote", "Number of TCP remote connections spawned");
    describe_counter!("ownserver_server.remote.tcp.received_bytes", "Bytes received over TCP connections");
    describe_counter!("ownserver_server.remote.tcp.sent_bytes", "Bytes sent over TCP connections");

    describe_counter!("ownserver_server.remote.udp.swawn_remote", "Number of UDP remote connections spawned");
    describe_counter!("ownserver_server.remote.udp.received_bytes", "Bytes received over UDP connections");
    describe_counter!("ownserver_server.remote.udp.sent_bytes", "Bytes sent over UDP connections");

    describe_counter!("ownserver_server.client.control_packet.received_bytes", "Bytes received in control packets");
    describe_counter!("ownserver_server.client.control_packet.sent_bytes", "Bytes sent in control packets");

    describe_gauge!("ownserver_server.store.clients", "Number of active clients");
    describe_gauge!("ownserver_server.store.streams", "Number of active streams");
    describe_gauge!("ownserver_server.stream.rtt", "Round-trip time between server and client in milliseconds");

    describe_counter!("ownserver_server.control_server.handle_new_connection", "[counter] The number of successfully accepted websocket connections so far.");
    describe_counter!("ownserver_server.control_server.handle_new_connection.read_client_hello_error", "[counter] The number of times the server failed to read request from clients.");
    describe_counter!("ownserver_server.control_server.handle_new_connection.send_server_hello_error", "[counter] The server tried to send error to clients, but failed.");
    describe_counter!("ownserver_server.control_server.handle_new_connection.reconnect.client_not_found", "[counter] The server received a reconnect request from clients, but the client was not found.");
    describe_counter!("ownserver_server.control_server.handle_new_connection.newclient.success", "[counter] The number of successfully processed new client requests.");
    describe_counter!("ownserver_server.control_server.handle_new_connection.reconnect.success", "[counter] The number of successfully processed reconnect requests.");
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

    let mut set = run(
        CONFIG.get().expect("failed to get config"),
        store,
    ).await;
    
    
    while let Some(res) = set.join_next().await {
        match res {
            Err(join_error) => {
                tracing::error!("join error {:?} for proxy_server", join_error);
            }
            Ok(_) => {
                tracing::info!("proxy_server successfully terminated");
            }
        }
    }
}
