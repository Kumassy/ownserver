use serial_test::serial;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::UdpSocket;
use ownserver_test::wait;

#[cfg(test)]
mod e2e_tcp_test {
    use super::*;
    use ownserver_test::{tcp::{with_proxy, with_local_server, get_endpoint_claims_single}, assert_tcp_socket_bytes_matches, LOCAL_PORT};


    #[tokio::test]
    #[serial]
    async fn forward_remote_traffic_to_local(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let endpoint_claims = get_endpoint_claims_single(LOCAL_PORT);
        with_proxy(endpoint_claims, |_token_server, _proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            with_local_server(LOCAL_PORT, |_local_server| async move {
                let mut remote = TcpStream::connect(remote_addr)
                    .await?;
                remote.write_all(b"foobar".as_ref()).await?;
                assert_tcp_socket_bytes_matches!(&mut remote, b"hello, foobar");

                Ok(())
            }).await;
            Ok(())
        }).await;

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_multiple_remote_traffic_to_local() -> Result<(), Box<dyn std::error::Error>> {
        let endpoint_claims = get_endpoint_claims_single(LOCAL_PORT);
        with_proxy(endpoint_claims, |_token_server, _proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            with_local_server(LOCAL_PORT, |_local_server| async move {
                let mut remote = TcpStream::connect(remote_addr.clone())
                    .await
                    .expect("Failed to connect to remote port");
                let mut remote2 = TcpStream::connect(remote_addr.clone())
                    .await
                    .expect("Failed to connect to remote port");
                wait!();

                remote.write_all(b"foobar".as_ref()).await?;
                assert_tcp_socket_bytes_matches!(&mut remote, b"hello, foobar");

                remote2.write_all(b"fugapiyo".as_ref()).await?;
                assert_tcp_socket_bytes_matches!(&mut remote2, b"hello, fugapiyo");

                Ok(())
            }).await;
            Ok(())
        }).await;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn refuse_remote_traffic_when_client_canceled() -> Result<(), Box<dyn std::error::Error>> {
        let endpoint_claims = get_endpoint_claims_single(LOCAL_PORT);
        with_proxy(endpoint_claims, |_token_server, _proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            with_local_server(LOCAL_PORT, |_local_server| async move {
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
                assert_tcp_socket_bytes_matches!(&mut remote, b"");

                Ok(())
            }).await;
            Ok(())
        }).await;

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn refuse_remote_traffic_after_client_canceled() -> Result<(), Box<dyn std::error::Error>> {
        let endpoint_claims = get_endpoint_claims_single(LOCAL_PORT);
        with_proxy(endpoint_claims, |_token_server, _proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            with_local_server(LOCAL_PORT, |_local_server| async move {
                let mut remote = TcpStream::connect(remote_addr)
                    .await?;
                wait!();

                remote.write_all(b"foobar".as_ref()).await?;
                assert_tcp_socket_bytes_matches!(&mut remote, b"hello, foobar");

                // cancel client
                let cancellation_token = proxy_client.cancellation_token;
                cancellation_token.cancel();

                remote.write_all(b"foobar".as_ref()).await?;
                // assert_tcp_socket_bytes_matches!(remote, b"");
                Ok(())
            }).await;

            Ok(())
        }).await;

        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn remove_disabled_client_streams() -> Result<(), Box<dyn std::error::Error>> {
        let endpoint_claims = get_endpoint_claims_single(LOCAL_PORT);
        with_proxy(endpoint_claims, |_token_server, proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            with_local_server(LOCAL_PORT, |_local_server| async move {
                let mut remote = TcpStream::connect(remote_addr)
                    .await?;
                wait!();

                remote.write_all(b"foobar".as_ref()).await?;
                assert_tcp_socket_bytes_matches!(&mut remote, b"hello, foobar");

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
        }).await;

        Ok(())
    }
}



#[cfg(test)]
mod e2e_udp_test {
    use ownserver_test::{udp::{with_proxy, with_local_server, get_endpoint_claims_single}, assert_udp_socket_bytes_matches, LOCAL_PORT};

    use super::*;

    #[tokio::test]
    #[serial]
    async fn forward_remote_traffic_to_local() -> Result<(), Box<dyn std::error::Error>> {
        let endpoint_claims = get_endpoint_claims_single(LOCAL_PORT);
        with_proxy(endpoint_claims, |_token_server, _proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            with_local_server(LOCAL_PORT, |_local_server| async move {
                let remote = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                    remote.connect(remote_addr).await.unwrap();
                remote.send(b"foobar".as_ref()).await?;
                wait!();

                assert_udp_socket_bytes_matches!(&remote, b"hello, foobar");

                Ok(())
            }).await;
            Ok(())
        }).await;
        Ok(())
    }

    #[tokio::test]
    #[serial]
    async fn forward_multiple_remote_traffic_to_local() -> Result<(), Box<dyn std::error::Error>> {
        let endpoint_claims = get_endpoint_claims_single(LOCAL_PORT);
        with_proxy(endpoint_claims, |_token_server, _proxy_server, proxy_client| async move {
            let client_info = proxy_client.client_info;
            let remote_addr = format!("{}:{}", client_info.host, client_info.endpoints[0].remote_port);
            wait!();

            with_local_server(LOCAL_PORT, |_local_server| async move {
                let remote1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                remote1.connect(remote_addr.clone()).await.unwrap();
                let remote2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                remote2.connect(remote_addr).await.unwrap();

                remote1.send(b"foobar".as_ref()).await?;
                remote2.send(b"fugapiyo".as_ref()).await?;
                wait!();

                assert_udp_socket_bytes_matches!(&remote1, b"hello, foobar");
                assert_udp_socket_bytes_matches!(&remote2, b"hello, fugapiyo");

                Ok(())
            }).await;
            Ok(())
        }).await;

        Ok(())
    }
}