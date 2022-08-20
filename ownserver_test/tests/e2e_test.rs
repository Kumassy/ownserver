use ownserver_lib::Payload;
use ownserver_server::Store;
use serial_test::serial;
use lazy_static::lazy_static;
use ownserver::{
    proxy_client::{self, ClientInfo},
    Store as ClientStore,
};
use ownserver_server::{
    port_allocator::PortAllocator,
    proxy_server,
    Config,
};
use ownserver_auth::build_routes;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot::{self, Receiver};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use once_cell::sync::OnceCell;
use tokio::net::UdpSocket;


#[cfg(test)]
mod e2e_tcp_test {
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

    async fn launch_token_server() {
        let hosts = vec![
            "127.0.0.1".to_string(),
        ];
        let routes = build_routes("supersecret".to_string(), hosts);

        tokio::spawn(async move {
            warp::serve(routes).run(([127, 0, 0, 1], 8888)).await;
        });
    }

    async fn launch_proxy_server(
        control_port: u16,
    ) -> Result<Arc<Store>, Box<dyn std::error::Error>> {

        let config = CONFIG.get_or_init(||
            Config {
                control_port,
                token_secret: "supersecret".to_string(),
                host: "127.0.0.1".to_string(),
                remote_port_start: 4500,
                remote_port_end: 4599,
            }
        );

        let alloc = Arc::new(Mutex::new(PortAllocator::new(config.remote_port_start..config.remote_port_end)));
        let store = Arc::new(Store::default());

        let store_ = store.clone();
        tokio::spawn(async move {
            proxy_server::run(
                &CONFIG,
                store_,
                alloc,
            )
            .await.unwrap();
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

        tokio::spawn(async move {
            let (client_info, handle) =
                proxy_client::run(client_store, control_port, local_port, "http://127.0.0.1:8888/v0/request_token", Payload::Other, cancellation_token)
                    .await
                    .expect("failed to launch proxy_client");
            tx.send(client_info).unwrap();

            handle.await.unwrap().unwrap();
        });

        Ok(rx)
    }

    async fn launch_local_server(local_port: u16) {
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
    }

    const CONTROL_PORT: u16 = 5000;
    const LOCAL_PORT: u16 = 3000;

    #[tokio::test]
    #[serial]
    async fn forward_remote_traffic_to_local() -> Result<(), Box<dyn std::error::Error>> {
        let cancellation_token = CancellationToken::new();

        launch_token_server().await;
        let store = launch_proxy_server(CONTROL_PORT).await?;
        wait!();

        launch_local_server(LOCAL_PORT).await;
        let client_info =
            launch_proxy_client(CONTROL_PORT, LOCAL_PORT, cancellation_token).await?;
        let remote_addr = client_info.await?.remote_addr;
        wait!();

        let mut remote = TcpStream::connect(remote_addr)
            .await
            .expect("Failed to connect to remote port");
        remote.write_all(b"foobar".as_ref()).await?;
        assert_socket_bytes_matches!(remote, b"hello, foobar");

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_multiple_remote_traffic_to_local() -> Result<(), Box<dyn std::error::Error>> {
        let cancellation_token = CancellationToken::new();

        launch_token_server().await;
        let store = launch_proxy_server(CONTROL_PORT).await?;
        wait!();

        launch_local_server(LOCAL_PORT).await;
        let client_info =
            launch_proxy_client(CONTROL_PORT, LOCAL_PORT, cancellation_token).await?;
        let remote_addr = client_info.await?.remote_addr;
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
    }

    #[tokio::test]
    #[serial]
    async fn refuse_remote_traffic_when_local_server_not_running(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cancellation_token = CancellationToken::new();

        launch_token_server().await;
        let store = launch_proxy_server(CONTROL_PORT).await?;
        wait!();

        let client_info =
            launch_proxy_client(CONTROL_PORT, LOCAL_PORT, cancellation_token).await?;
        let remote_addr = client_info.await?.remote_addr;
        wait!();

        let mut remote = TcpStream::connect(remote_addr)
            .await
            .expect("Failed to connect to remote port");
        wait!();

        assert_socket_bytes_matches!(remote, b"");

        // store owns remote stream
        // remote stream never closes unless we release its ownership here
        store.cleanup();

        // at the first write operation, proxy_server send FIN
        remote.write_all(b"foobar".as_ref()).await?;
        remote.flush().await?;
        wait!();

        assert!(
            remote.write_all(b"foobar".as_ref()).await.is_err(),
            "second operation must fail because proxy_server release tcp connection."
        );
        
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn refuse_remote_traffic_when_remote_process_not_running(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let remote_port: u16 = 8080;

        launch_token_server().await;
        let store = launch_proxy_server(CONTROL_PORT).await?;
        wait!();

        let remote = TcpStream::connect(format!("127.0.0.1:{}", remote_port)).await;
        assert!(remote.is_err());

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn refuse_remote_traffic_when_client_canceled() -> Result<(), Box<dyn std::error::Error>> {
        let cancellation_token = CancellationToken::new();

        launch_token_server().await;
        let store = launch_proxy_server(CONTROL_PORT).await?;
        wait!();

        launch_local_server(LOCAL_PORT).await;
        let client_info =
            launch_proxy_client(CONTROL_PORT, LOCAL_PORT, cancellation_token.clone()).await?;
        let remote_addr = client_info.await?.remote_addr;
        wait!();
        
        // cancel client
        cancellation_token.cancel();


        // access remote port
        // we can access remote server because remote_port_for_client remains open
        // even if client has cancelled
        let mut remote = TcpStream::connect(remote_addr)
            .await
            .expect("Failed to connect to remote port");
        wait!();

        remote.write_all(b"foobar".as_ref()).await?;
        assert_socket_bytes_matches!(remote, b"");

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn refuse_remote_traffic_after_client_canceled() -> Result<(), Box<dyn std::error::Error>> {
        let cancellation_token = CancellationToken::new();

        launch_token_server().await;
        let store = launch_proxy_server(CONTROL_PORT).await?;
        wait!();

        launch_local_server(LOCAL_PORT).await;
        let client_info =
            launch_proxy_client(CONTROL_PORT, LOCAL_PORT, cancellation_token.clone()).await?;
        let remote_addr = client_info.await?.remote_addr;
        wait!();

        let mut remote = TcpStream::connect(remote_addr)
            .await
            .expect("Failed to connect to remote port");
        wait!();

        remote.write_all(b"foobar".as_ref()).await?;
        assert_socket_bytes_matches!(remote, b"hello, foobar");

        // cancel client
        cancellation_token.cancel();
        wait!();

        remote.write_all(b"foobar".as_ref()).await?;
        // assert_socket_bytes_matches!(remote, b"");

        Ok(())
    }
}



#[cfg(test)]
mod e2e_udp_test {
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
                control_port: 5000,
                token_secret: "supersecret".to_string(),
                host: "127.0.0.1".to_string(),
                remote_port_start: 4600,
                remote_port_end: 4699,
            }
        );
        let alloc = Arc::new(Mutex::new(PortAllocator::new(config.remote_port_start..config.remote_port_end)));
        let store = Arc::new(Store::default());

        let store_ = store.clone();
        tokio::spawn(async move {
            proxy_server::run(
                &CONFIG,
                store_,
                alloc,
            )
            .await.unwrap();
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

        tokio::spawn(async move {
            let (client_info, handle) =
                proxy_client::run(client_store, control_port, local_port, "http://127.0.0.1:8888/v0/request_token", Payload::UDP, cancellation_token)
                    .await
                    .expect("failed to launch proxy_client");
            tx.send(client_info).unwrap();

            handle.await.unwrap().unwrap();
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
        let store = launch_proxy_server(CONTROL_PORT).await?;
        wait!();

        launch_local_server(LOCAL_PORT).await;
        let client_info =
            launch_proxy_client(CONTROL_PORT, LOCAL_PORT, cancellation_token).await?;
        let remote_addr = client_info.await?.remote_addr;
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
        let store = launch_proxy_server(CONTROL_PORT).await?;
        wait!();

        launch_local_server(LOCAL_PORT).await;
        let client_info =
            launch_proxy_client(CONTROL_PORT, LOCAL_PORT, cancellation_token).await?;
        let remote_addr = client_info.await?.remote_addr;
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