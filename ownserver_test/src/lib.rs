use std::sync::Arc;
use chrono::Duration;

use ownserver_auth::build_routes;
use ownserver_server::{
    proxy_server,
    Store,
};
use ownserver_lib::{EndpointClaim, EndpointClaims, Protocol};
use ownserver::{
    proxy_client::{self, ClientInfo},
    Store as ClientStore,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tokio::net::UdpSocket;
use futures::Future;

mod ports;
pub use ports::{use_ports, PortSet};
use warp::filters::ws::WebSocket;

#[macro_export]
macro_rules! wait {
    () => {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        
    };
}


#[macro_export]
macro_rules! assert_tcp_socket_bytes_matches {
    ($socket:expr, $expected:expr) => {
        let mut buf = [0; 4 * 1024];
        let n = $socket
            .read(&mut buf)
            .await
            .expect("failed to read data from socket");
        let data = buf[..n].to_vec();

        assert_eq!(data, $expected);  
    };
}

#[macro_export]
macro_rules! assert_udp_socket_bytes_matches {
    ($socket:expr, $expected:expr) => {
        let mut buf = [0; 4 * 1024];
        let n = $socket
            .recv(&mut buf)
            .await
            .expect("failed to read data from socket");
        let data = buf[..n].to_vec();

        assert_eq!(data, $expected);  
    };
}

pub struct TokenServer {
}


pub struct ProxyServer {
    pub store: Arc<Store<WebSocket>>,
}

pub struct ProxyClient {
    pub store: Arc<ClientStore>,
    pub client_info: ClientInfo,
    pub cancellation_token: CancellationToken,
}

impl ProxyClient {
    pub async fn cancel(&self) {
        self.cancellation_token.cancel();
        self.store.remove_client().await;
    }
}

    
pub struct LocalServer {
    
}

pub async fn launch_token_server(token_port: u16) -> TokenServer {
    let hosts = vec![
        "127.0.0.1".to_string(),
    ];
    let routes = build_routes("supersecret".to_string(), hosts);

    tokio::spawn(async move {
        warp::serve(routes).run(([127, 0, 0, 1], token_port)).await;
    });

    wait!();
    TokenServer {}
}


pub async fn launch_proxy_server(
    control_port: u16,
    remote_port_start: u16,
    remote_port_end: u16,
    ping_interval: Option<u64>,
) -> Result<ProxyServer, Box<dyn std::error::Error>> {
    let periodic_ping_interval = ping_interval.unwrap_or(2 << 30);
    let config = Box::leak(Box::new(ownserver_server::Config {
        control_port,
        token_secret: "supersecret".to_string(),
        host: "127.0.0.1".to_string(),
        remote_port_start,
        remote_port_end,
        periodic_cleanup_interval: 2 << 30,
        periodic_ping_interval,
        reconnect_window: Duration::seconds(2),
    }));

    let store = Arc::new(Store::new(config.remote_port_start..config.remote_port_end));

    let store_ = store.clone();
    tokio::spawn(async move {
        proxy_server::run(
            config,
            store_,
        )
        .await.join_next().await;
    });

    wait!();
    Ok(ProxyServer {
        store,
    })
}

pub mod tcp {

    use proxy_client::RequestType;

    use super::*;

    pub fn get_endpoint_claims_single(local_port: u16) -> EndpointClaims {
        vec![EndpointClaim {
            protocol: Protocol::TCP,
            local_port,
            remote_port: 0,
        }]
    }
    
    pub async fn launch_proxy_client(
        token_port: u16,
        control_port: u16,
        request_type: RequestType,
    ) -> Result<ProxyClient, Box<dyn std::error::Error>> {
        let client_store: Arc<ClientStore> = Default::default();
        let cancellation_token = CancellationToken::new();
    
        let (client_info, mut set) =
                proxy_client::run(client_store.clone(), control_port, &format!("http://127.0.0.1:{}/v0/request_token", token_port), cancellation_token.clone(), 15, request_type)
                    .await
                    .expect("failed to launch proxy_client");
        tokio::spawn(async move {
            while let Some(res) = set.join_next().await {
                let _ = res.unwrap();
            }
        });
    
        wait!();
        Ok(ProxyClient {
            store: client_store,
            client_info,
            cancellation_token,
        })
    }

    pub async fn launch_proxy_client_reconnect(
        client_store: Arc<ClientStore>,
        control_port: u16,
        token: String,
        host: String,
        request_type: RequestType,
    ) -> Result<ProxyClient, Box<dyn std::error::Error>> {
        let cancellation_token = CancellationToken::new();
    
        let (client_info, mut set) =
                proxy_client::run_with_token(client_store.clone(), control_port, cancellation_token.clone(), 15, token, host, request_type)
                    .await
                    .expect("failed to launch proxy_client");
        tokio::spawn(async move {
            while let Some(res) = set.join_next().await {
                let _ = res.unwrap();
            }
        });
    
        wait!();
        Ok(ProxyClient {
            store: client_store,
            client_info,
            cancellation_token,
        })
    }

    pub async fn launch_proxy_client_new(
        token_port: u16,
        control_port: u16,
        request_type: RequestType,
    ) -> Result<ProxyClient, Box<dyn std::error::Error>> {
        let config = Box::leak(Box::new(ownserver::Config {
            control_port,
            token_server: format!("http://127.0.0.1:{}/v0/request_token", token_port),
            ping_interval: 15,
        }));

        let client_store: Arc<ClientStore> = Default::default();
        let cancellation_token = CancellationToken::new();
    
        let client_store_ = client_store.clone();
        let cancellation_token_ = cancellation_token.clone();
        tokio::spawn(async move {
            proxy_client::new_run_client(config, client_store_.clone(), cancellation_token_, request_type)
                .await
                .expect("failed to launch proxy_client");
            tracing::warn!("new_run_client finished");
        });
    
        for _ in 0..10 {
            wait!();
            if let Some(client_info) = client_store.get_client_info().await {
                return Ok(ProxyClient {
                    store: client_store,
                    client_info,
                    cancellation_token,
                });
            }
        }
        Err("failed to get client info".into())
    }

    pub async fn launch_local_server(local_port: u16) -> LocalServer {
        let local_server = async move {
            let listener = TcpListener::bind(format!("127.0.0.1:{}", local_port))
                .await
                .unwrap();
    
            loop {
                let (mut socket, _) = listener.accept().await.expect("No connections to accept");
    
                tokio::spawn(async move {
                    loop {
                        let mut buf = [0; 4 * 1024];
                        let n = socket
                            .read(&mut buf)
                            .await
                            .expect("failed to read data from socket");
                        if n == 0 {
                            return;
                        }
    
                        let mut msg = b"hello, ".to_vec();
                        msg.append(&mut buf[..n].to_vec());
                        socket
                            .write_all(&msg)
                            .await
                            .expect("failed to write packet data to local tcp socket");
                    }
                });
            }
        };
        tokio::spawn(local_server);
    
        wait!();
        LocalServer {}
    }

    pub async fn launch_local_server_echoback(local_port: u16) -> LocalServer {
        let local_server = async move {
            let listener = TcpListener::bind(format!("127.0.0.1:{}", local_port))
                .await
                .unwrap();
    
            loop {
                let (mut socket, _) = listener.accept().await.expect("No connections to accept");
    
                tokio::spawn(async move {
                    loop {
                        let mut buf = [0; 4 * 1024];
                        let n = socket
                            .read(&mut buf)
                            .await
                            .expect("failed to read data from socket");
                        if n == 0 {
                            return;
                        }
    
                        socket
                            .write_all(&buf[..n])
                            .await
                            .expect("failed to write packet data to local tcp socket");
                    }
                });
            }
        };
        tokio::spawn(local_server);
    
        wait!();
        LocalServer {}
    }


    async fn _with_proxy<T>(ping_interval: Option<u64>, port_set: PortSet, request_type: RequestType, test_func: impl FnOnce(TokenServer, ProxyServer, ProxyClient) -> T)
        where
        T: Future<Output = Result<(), Box<dyn std::error::Error>>> + Send,
    {
        let token_server = launch_token_server(port_set.token_port).await;
        wait!();

        let proxy_server = launch_proxy_server(port_set.control_port, port_set.remote_ports.start, port_set.remote_ports.end, ping_interval).await.expect("failed to launch proxy server");
        wait!();

        let proxy_client = launch_proxy_client(port_set.token_port, port_set.control_port, request_type).await.expect("failed to launch proxy client");

        test_func(token_server, proxy_server, proxy_client).await.expect("failed to call test_func");
    }


    pub async fn with_proxy<T>(port_set: PortSet, request_type: RequestType, test_func: impl FnOnce(TokenServer, ProxyServer, ProxyClient) -> T)
        where
        T: Future<Output = Result<(), Box<dyn std::error::Error>>> + Send,
    {
        _with_proxy(None, port_set, request_type, test_func).await;
    }

    pub async fn with_proxy_use_reconnect<T>(port_set: PortSet, request_type: RequestType, test_func: impl FnOnce(TokenServer, ProxyServer, ProxyClient) -> T)
        where
        T: Future<Output = Result<(), Box<dyn std::error::Error>>> + Send,
    {
        _with_proxy(Some(1), port_set, request_type, test_func).await;
    }


    pub async fn with_local_server<T>(local_port: u16, test_func: impl FnOnce(LocalServer) -> T)
        where
        T: Future<Output = Result<(), Box<dyn std::error::Error>>> + Send,
    {
        let local_server = launch_local_server(local_port).await;
        wait!();

        test_func(local_server).await.expect("failed to call test_func");
    }

    pub async fn with_local_server_echoback<T>(local_port: u16, test_func: impl FnOnce(LocalServer) -> T)
        where
        T: Future<Output = Result<(), Box<dyn std::error::Error>>> + Send,
    {
        let local_server = launch_local_server_echoback(local_port).await;
        wait!();

        test_func(local_server).await.expect("failed to call test_func");
    }
}

pub mod udp {
    use proxy_client::RequestType;

    use super::*;

    pub fn get_endpoint_claims_single(local_port: u16) -> EndpointClaims {
        vec![EndpointClaim {
            protocol: Protocol::UDP,
            local_port,
            remote_port: 0,
        }]
    }

    pub async fn launch_proxy_client(
        token_port: u16,
        control_port: u16,
        request_type: RequestType,
    ) -> Result<ProxyClient, Box<dyn std::error::Error>> {
        let client_store: Arc<ClientStore> = Default::default();
        let cancellation_token = CancellationToken::new();
    
        let (client_info, mut set) =
            proxy_client::run(client_store.clone(), control_port, &format!("http://127.0.0.1:{}/v0/request_token", token_port), cancellation_token.clone(), 15, request_type)
                .await
                .expect("failed to launch proxy_client");
        tokio::spawn(async move {
            while let Some(res) = set.join_next().await {
                let _ = res.unwrap();
            }
        });
    
        wait!();
        Ok(ProxyClient {
            store: client_store,
            client_info,
            cancellation_token,
        })
    }
    
    pub async fn launch_local_server(local_port: u16) -> LocalServer {
        let local_server = async move {
            let socket = UdpSocket::bind(format!("127.0.0.1:{}", local_port)).await.unwrap();
    
            tokio::spawn(async move {
                loop {
                    let mut buf = [0; 4 * 1024];
                    let (n, addr) = socket
                        .recv_from(&mut buf)
                        .await
                        .expect("failed to read data from socket");
                    if n == 0 {
                        return;
                    }
    
    
                    let mut msg = b"hello, ".to_vec();
                    msg.append(&mut buf[..n].to_vec());
                    tracing::info!("send data to remote socket: {:?}", msg);
                    socket
                        .send_to(&msg, addr)
                        .await
                        .expect("failed to write packet data to local udp socket");
                }
            });
        };
        tokio::spawn(local_server);

        wait!();
        LocalServer {}
    }


    pub async fn with_proxy<T>(port_set: PortSet, request_type: RequestType, test_func: impl FnOnce(TokenServer, ProxyServer, ProxyClient) -> T)
        where
        T: Future<Output = Result<(), Box<dyn std::error::Error>>> + Send,
    {
        let token_server = launch_token_server(port_set.token_port).await;
        wait!();

        let proxy_server = launch_proxy_server(port_set.control_port, port_set.remote_ports.start, port_set.remote_ports.end, None).await.expect("failed to launch proxy server");
        wait!();

        let proxy_client = launch_proxy_client(port_set.token_port, port_set.control_port, request_type).await.expect("failed to launch proxy client");

        test_func(token_server, proxy_server, proxy_client).await.expect("failed to call test_func");
    }

    pub async fn with_local_server<T>(local_port: u16, test_func: impl FnOnce(LocalServer) -> T)
        where
        T: Future<Output = Result<(), Box<dyn std::error::Error>>> + Send,
    {
        let local_server = launch_local_server(local_port).await;
        wait!();

        test_func(local_server).await.expect("failed to call test_func");
    }
}

