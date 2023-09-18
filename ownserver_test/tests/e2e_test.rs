use ownserver_server::Store;
use serial_test::serial;
use ownserver::{
    proxy_client::{self, ClientInfo},
    Store as ClientStore,
};
use ownserver_server::{
    proxy_server,
    Config,
};
use ownserver_auth::build_routes;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot::{self, Receiver};
use tokio_util::sync::CancellationToken;
use once_cell::sync::OnceCell;
use tokio::net::UdpSocket;

#[cfg(test)]
mod e2e_tcp_test {
    use futures::Future;
    use ownserver_lib::{EndpointClaim, Protocol};

    use super::*;

    static CONFIG: OnceCell<Config> = OnceCell::new();

    macro_rules! assert_socket_bytes_matches {
        ($read:expr, $expected:expr) => {
            let mut buf = [0; 4 * 1024];
            let n = $read
                .read(&mut buf)
                .await
                .expect("failed to read data from socket");
            let data = buf[..n].to_vec();

            assert_eq!(data, $expected);
        };
    }

    macro_rules! wait {
        () => {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    struct TokenServer {
    }

    async fn launch_token_server() -> TokenServer {
        let hosts = vec![
            "127.0.0.1".to_string(),
        ];
        let routes = build_routes("supersecret".to_string(), hosts);

        tokio::spawn(async move {
            warp::serve(routes).run(([127, 0, 0, 1], 8888)).await;
        });

        wait!();
        TokenServer {}
    }

    struct ProxyServer {
        store: Arc<Store>,
    }

    async fn launch_proxy_server(
        control_port: u16,
        remote_port_start: u16,
        remote_port_end: u16
    ) -> Result<ProxyServer, Box<dyn std::error::Error>> {

        let config = CONFIG.get_or_init(||
            Config {
                control_port,
                token_secret: "supersecret".to_string(),
                host: "127.0.0.1".to_string(),
                remote_port_start,
                remote_port_end,
                periodic_cleanup_interval: 2 << 30,
                periodic_ping_interval: 2 << 30,
            }
        );

        let store = Arc::new(Store::new(config.remote_port_start..config.remote_port_end));

        let store_ = store.clone();
        tokio::spawn(async move {
            proxy_server::run(
                &CONFIG,
                store_,
            )
            .await.join_next().await;
        });

        wait!();
        Ok(ProxyServer {
            store,
        })
    }

    struct ProxyClient {
        client_info: ClientInfo,
        cancellation_token: CancellationToken,
    }

    async fn launch_proxy_client(
        control_port: u16,
        local_port: u16,
    ) -> Result<ProxyClient, Box<dyn std::error::Error>> {
        let client_store: Arc<ClientStore> = Default::default();
        let cancellation_token = CancellationToken::new();

        let endpoint_claims = vec![EndpointClaim {
            protocol: Protocol::TCP,
            local_port,
            remote_port: 0,
        }];

        let (client_info, mut set) =
                proxy_client::run(client_store, control_port, "http://127.0.0.1:8888/v0/request_token", cancellation_token.clone(), endpoint_claims)
                    .await
                    .expect("failed to launch proxy_client");
        tokio::spawn(async move {
            while let Some(res) = set.join_next().await {
                let _ = res.unwrap();
            }
        });

        wait!();
        Ok(ProxyClient {
            client_info,
            cancellation_token,
        })
    }

    struct LocalServer {

    }
    async fn launch_local_server(local_port: u16) -> LocalServer {
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

    const CONTROL_PORT: u16 = 5000;
    const LOCAL_PORT: u16 = 3000;
    const REMOTE_PORT_START: u16 = 4500;
    const REMOTE_PORT_END: u16 = 4599;

    async fn launch_all<T>(test_func: impl FnOnce(TokenServer, ProxyServer, LocalServer, ProxyClient) -> T)
        where
        T: Future<Output = Result<(), Box<dyn std::error::Error>>> + Send,
    {
        let token_server = launch_token_server().await;
        wait!();

        let proxy_server = launch_proxy_server(CONTROL_PORT, REMOTE_PORT_START, REMOTE_PORT_END).await.expect("failed to launch proxy server");
        let local_server = launch_local_server(LOCAL_PORT).await;

        wait!();
        let proxy_client = launch_proxy_client(CONTROL_PORT, LOCAL_PORT).await.expect("failed to launch proxy client");

        test_func(token_server, proxy_server, local_server, proxy_client).await.expect("failed to call test_func");
    }

    #[tokio::test]
    #[serial]
    async fn forward_remote_traffic_to_local(
    ) -> Result<(), Box<dyn std::error::Error>> {
        launch_all(|_token_server, _proxy_server, _local_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            let mut remote = TcpStream::connect(remote_addr)
                .await?;
            remote.write_all(b"foobar".as_ref()).await?;
            assert_socket_bytes_matches!(remote, b"hello, foobar");

            Ok(())
        }).await;

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_multiple_remote_traffic_to_local() -> Result<(), Box<dyn std::error::Error>> {
        launch_all(|_token_server, _proxy_server, _local_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            let mut remote = TcpStream::connect(remote_addr.clone())
                .await
                .expect("Failed to connect to remote port");
            let mut remote2 = TcpStream::connect(remote_addr.clone())
                .await
                .expect("Failed to connect to remote port");
            wait!();

            remote.write_all(b"foobar".as_ref()).await?;
            assert_socket_bytes_matches!(remote, b"hello, foobar");

            remote2.write_all(b"fugapiyo".as_ref()).await?;
            assert_socket_bytes_matches!(remote2, b"hello, fugapiyo");

            Ok(())
        }).await;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn refuse_remote_traffic_when_client_canceled() -> Result<(), Box<dyn std::error::Error>> {
        launch_all(|_token_server, _proxy_server, _local_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            // cancel client
            let cancellation_token = proxy_client.cancellation_token;
            cancellation_token.cancel();

            // access remote port
            // we can access remote server because remote_port_for_client remains open
            // even if client has cancelled
            let mut remote = TcpStream::connect(remote_addr)
                .await?;
            wait!();

            remote.write_all(b"foobar".as_ref()).await?;
            assert_socket_bytes_matches!(remote, b"");

            Ok(())
        }).await;

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn refuse_remote_traffic_after_client_canceled() -> Result<(), Box<dyn std::error::Error>> {
        launch_all(|_token_server, _proxy_server, _local_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            let mut remote = TcpStream::connect(remote_addr)
                .await?;
            wait!();

            remote.write_all(b"foobar".as_ref()).await?;
            assert_socket_bytes_matches!(remote, b"hello, foobar");

            // cancel client
            let cancellation_token = proxy_client.cancellation_token;
            cancellation_token.cancel();

            remote.write_all(b"foobar".as_ref()).await?;
            // assert_socket_bytes_matches!(remote, b"");

            Ok(())


        }).await;

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn remove_disabled_client_streams() -> Result<(), Box<dyn std::error::Error>> {
        launch_all(|_token_server, proxy_server, _local_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            let mut remote = TcpStream::connect(remote_addr)
                .await?;
            wait!();

            remote.write_all(b"foobar".as_ref()).await?;
            assert_socket_bytes_matches!(remote, b"hello, foobar");

            let store = proxy_server.store;

            // now 1 client, 1 stream
            assert_eq!(store.len_clients().await, 1);
            assert_eq!(store.len_streams().await, 1);

            let cancellation_token = proxy_client.cancellation_token;
            cancellation_token.cancel();
            wait!();

            // client and stream remains in store
            assert_eq!(store.len_clients().await, 1);
            assert_eq!(store.len_streams().await, 1);

            // need to call cleanup
            store.cleanup().await;
            wait!();
            assert_eq!(store.len_clients().await, 0);
            assert_eq!(store.len_streams().await, 0);

            Ok(())
        }).await;

        Ok(())
    }
}



#[cfg(test)]
mod e2e_udp_test {
    use ownserver_lib::{EndpointClaim, Protocol};

    use super::*;


    static CONFIG: OnceCell<Config> = OnceCell::new();

    macro_rules! assert_socket_bytes_matches {
        ($read:expr, $expected:expr) => {
            let mut buf = [0; 4 * 1024];
            let n = $read
                .recv(&mut buf)
                .await
                .expect("failed to read data from socket");
            let data = buf[..n].to_vec();

            assert_eq!(data, $expected);
        };
    }

    macro_rules! wait {
        () => {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    async fn launch_token_server() {
        let hosts = vec![
            "127.0.0.1".to_string(),
        ];
        let routes = build_routes("supersecret".to_string(), hosts);

        tokio::spawn(async move {
            warp::serve(routes).run(([127, 0, 0, 1], 8888)).await;
        });
    }

    async fn launch_proxy_server(control_port: u16) -> Result<Arc<Store>, Box<dyn std::error::Error>> {
        let config = CONFIG.get_or_init(||
            Config {
                control_port,
                token_secret: "supersecret".to_string(),
                host: "127.0.0.1".to_string(),
                remote_port_start: 4600,
                remote_port_end: 4699,
                periodic_cleanup_interval: 2 << 30,
                periodic_ping_interval: 2 << 30,
            }
        );
        let store = Arc::new(Store::new(config.remote_port_start..config.remote_port_end));

        let store_ = store.clone();
        tokio::spawn(async move {
            proxy_server::run(
                &CONFIG,
                store_,
            )
            .await.join_next().await;
        });
        Ok(store)
    }

    async fn launch_proxy_client(
        control_port: u16,
        local_port: u16,
        cancellation_token: CancellationToken,
    ) -> Result<Receiver<ClientInfo>, Box<dyn std::error::Error>> {
        let client_store: Arc<ClientStore> = Default::default();
        let (tx, rx) = oneshot::channel();

        let endpoint_claims = vec![EndpointClaim {
            protocol: Protocol::UDP,
            local_port,
            remote_port: 0,
        }];
        tokio::spawn(async move {
            let (client_info, mut set) =
                proxy_client::run(client_store, control_port, "http://127.0.0.1:8888/v0/request_token", cancellation_token, endpoint_claims)
                    .await
                    .expect("failed to launch proxy_client");
            tx.send(client_info).unwrap();

            while let Some(res) = set.join_next().await {
                let _ = res.unwrap();
            }
        });

        Ok(rx)
    }

    async fn launch_local_server(local_port: u16) {
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
    }

    const CONTROL_PORT: u16 = 5000;
    const LOCAL_PORT: u16 = 3000;

    #[tokio::test]
    #[serial]
    async fn forward_remote_traffic_to_local() -> Result<(), Box<dyn std::error::Error>> {
        let cancellation_token = CancellationToken::new();

        launch_token_server().await;
        let _store = launch_proxy_server(CONTROL_PORT).await?;
        wait!();

        launch_local_server(LOCAL_PORT).await;
        let client_info =
            launch_proxy_client(CONTROL_PORT, LOCAL_PORT, cancellation_token).await?.await?;
        let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
        wait!();

        let remote = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            remote.connect(remote_addr).await.unwrap();
        remote.send(b"foobar".as_ref()).await?;
        wait!();

        assert_socket_bytes_matches!(remote, b"hello, foobar");

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_multiple_remote_traffic_to_local() -> Result<(), Box<dyn std::error::Error>> {
        let cancellation_token = CancellationToken::new();

        launch_token_server().await;
        let _store = launch_proxy_server(CONTROL_PORT).await?;
        wait!();

        launch_local_server(LOCAL_PORT).await;
        let client_info =
            launch_proxy_client(CONTROL_PORT, LOCAL_PORT, cancellation_token).await?.await?;
        let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
        wait!();

        let remote1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        remote1.connect(remote_addr.clone()).await.unwrap();
        let remote2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        remote2.connect(remote_addr).await.unwrap();

        remote1.send(b"foobar".as_ref()).await?;
        remote2.send(b"fugapiyo".as_ref()).await?;
        wait!();

        assert_socket_bytes_matches!(remote1, b"hello, foobar");
        assert_socket_bytes_matches!(remote2, b"hello, fugapiyo");

        Ok(())
    }
}