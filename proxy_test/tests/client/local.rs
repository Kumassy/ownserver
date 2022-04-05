#[cfg(test)]
mod client_local_test {
    use super::*;

    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn process_local_tcp_test() {
        let (service_addr_tx, service_addr_rx) = oneshot::channel();
        let (notify_exit_from_fn_tx, notify_exit_from_fn_rx) = oneshot::channel();
        let (tunnel_tx, mut tunnel_rx) = unbounded();
        let stream_id = StreamId::generate();
        let stream_id_clone = stream_id.clone();
        let proxy_client_service = async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            service_addr_tx
                .send(listener.local_addr().unwrap())
                .unwrap();
            let (stream, _) = listener.accept().await.expect("No connections to accept");
            let (reader, _writer) = split(stream);

            let _ = process_local_tcp(reader, tunnel_tx, stream_id_clone).await;
            notify_exit_from_fn_tx.send(()).unwrap();
        };
        tokio::spawn(proxy_client_service);

        info!("Waiting for server to be ready");
        let service_addr = service_addr_rx.await.expect("Server not ready");

        let mut local_service = TcpStream::connect(service_addr)
            .await
            .expect("Failed to connect");
        local_service
            .write_all(b"some bytes")
            .await
            .expect("failed to send client hello");
        local_service
            .shutdown()
            .await
            .expect("failed to close stream");

        // process_local_tcp should exit if TcpStream is closed
        let _ = notify_exit_from_fn_rx
            .await
            .expect("Failed to receive messages");

        // TcpReader data should be present in tunnel_rx
        assert_eq!(
            tunnel_rx.next().await,
            Some(ControlPacket::Data(
                stream_id.clone(),
                b"some bytes".to_vec()
            ))
        );
        assert_eq!(tunnel_rx.next().await, None);
    }

    #[tokio::test]
    async fn forward_to_local_tcp_test() {
        let (service_addr_tx, service_addr_rx) = oneshot::channel();
        let (mut msg_tx, mut msg_rx) = unbounded();
        let (mut tunnel_tx, tunnel_rx) = unbounded();
        let stream_id = StreamId::generate();
        let proxy_client_service = async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            service_addr_tx
                .send(listener.local_addr().unwrap())
                .unwrap();
            let (stream, _) = listener.accept().await.expect("No connections to accept");
            let (mut reader, _writer) = split(stream);

            let mut buf = [0; 4 * 1024];
            loop {
                let n = reader
                    .read(&mut buf)
                    .await
                    .expect("failed to read data from socket");
                if n == 0 {
                    info!("done reading from client stream");
                    return;
                }
                let data = buf[..n].to_vec();
                msg_tx.send(data).await.expect("failed to write to msg_tx");
            }
        };
        tokio::spawn(proxy_client_service);

        tunnel_tx
            .send(StreamMessage::Data(b"some bytes".to_vec()))
            .await
            .expect("failed to write to tunnel");
        drop(tunnel_tx);

        info!("Waiting for server to be ready");
        let service_addr = service_addr_rx.await.expect("Server not ready");

        let stream = TcpStream::connect(service_addr)
            .await
            .expect("Failed to connect");
        let (_reader, writer) = split(stream);
        // process_local_tcp should exit if TcpStream is closed
        let _ = forward_to_local_tcp(stream_id, writer, tunnel_rx).await;

        // Data sent to queue should be wrote to TcpStream
        assert_eq!(msg_rx.next().await, Some(b"some bytes".to_vec()));
        assert_eq!(msg_rx.next().await, None);
    }
}