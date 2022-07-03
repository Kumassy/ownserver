use crate::{connected_clients::ConnectedClient};
use dashmap::{DashMap, iter::Iter, mapref::one::{Ref, RefMut}};
use futures::{channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender, SendError}, stream, SinkExt, StreamExt, future::Remote};
use magic_tunnel_lib::{StreamId, ControlPacket, ClientId};
use metrics::gauge;
use tokio::{io::{AsyncWrite, AsyncWriteExt, AsyncRead, AsyncReadExt}, net::UdpSocket};
use std::{sync::Arc, net::SocketAddr};
use thiserror::Error;

pub const HTTP_ERROR_LOCATING_HOST_RESPONSE: &'static [u8] =
    b"HTTP/1.1 500\r\nContent-Length: 27\r\n\r\nError: Error finding tunnel";
pub const HTTP_NOT_FOUND_RESPONSE: &'static [u8] =
    b"HTTP/1.1 404\r\nContent-Length: 23\r\n\r\nError: Tunnel Not Found";
pub const HTTP_TUNNEL_REFUSED_RESPONSE: &'static [u8] =
    b"HTTP/1.1 500\r\nContent-Length: 32\r\n\r\nTunnel says: connection refused.";


#[derive(Debug, Clone)]
pub struct ActiveStream {
    pub id: StreamId,
    client: ConnectedClient,
    tx: UnboundedSender<StreamMessage>,
    is_disabled: bool,
}

impl ActiveStream {
    pub fn new(client: ConnectedClient) -> (Self, UnboundedReceiver<StreamMessage>) {
        let (tx, rx) = unbounded();
        (
            ActiveStream {
                id: StreamId::new(),
                client,
                tx,
                is_disabled: false,
            },
            rx,
        )
    }

    pub fn build_tcp<T>(client: ConnectedClient, mut sink: T, active_streams: ActiveStreams) -> Self where T: AsyncWrite + AsyncWriteExt + Unpin + Send + 'static {
        let (tx, mut rx) = unbounded();
        let client_id = client.id;
        let stream_id = StreamId::new();

        let host = client.host.clone();
        tokio::spawn(async move {
            loop {
                let result = rx.next().await;
        
                let result = if let Some(message) = result {
                    match message {
                        StreamMessage::Data(data) => Some(data),
                        StreamMessage::TunnelRefused => {
                            tracing::debug!(remote = %host, sid = %stream_id, "tunnel refused");
                            let _ = sink.write_all(HTTP_TUNNEL_REFUSED_RESPONSE).await;
                            None
                        }
                        StreamMessage::NoClientTunnel => {
                            tracing::info!(remote = %host, sid = %stream_id, "client tunnel not found");
                            let _ = sink.write_all(HTTP_NOT_FOUND_RESPONSE).await;
                            None
                        }
                    }
                } else {
                    None
                };
        
                let data = match result {
                    Some(data) => data,
                    None => {
                        tracing::debug!(remote = %host, sid = %stream_id, "done tunneling to sink");
                        let _ = sink.shutdown().await.map_err(|_e| {
                            tracing::error!(remote = %host, sid = %stream_id, "error shutting down tcp stream");
                        });
        
                        break
                    }
                };
        
                let result = sink.write_all(&data).await;
        
                if let Some(error) = result.err() {
                    tracing::warn!(remote = %host, sid = %stream_id, "stream closed, disconnecting: {:?}", error);
                    break
                }
            }

            tracing::debug!(
                remote = %host, cid = %client_id, sid = %stream_id, "tunnel_to_stream closed"
            );
            if let Some(mut stream) = active_streams.get_mut(&stream_id) {
                stream.disable();
            }
            active_streams.remove(&stream_id);
            gauge!("magic_tunnel_server.remotes.streams", active_streams.len() as f64);
            tracing::debug!(cid = %client_id, sid = %stream_id, "remove stream from active_streams, tunnel_to_stream len={}", active_streams.len());
        });

        ActiveStream {
            id: stream_id,
            client,
            tx,
            is_disabled: false,
        }
    }

    pub fn build_udp(client: ConnectedClient, socket: Arc<UdpSocket>, peer_addr: SocketAddr, active_streams: ActiveStreams) -> Self {
        let (tx, mut rx) = unbounded();
        let client_id = client.id;
        let stream_id = StreamId::new();
        let host = client.host.clone();

        tokio::spawn(async move {
            loop {
                // TODO: cancellation token
                let result = rx.next().await;
        
                let result = if let Some(message) = result {
                    match message {
                        StreamMessage::Data(data) => Some(data),
                        StreamMessage::TunnelRefused => {
                            tracing::debug!(remote = %host, sid = %stream_id, "tunnel refused");
                            let _ = socket.send_to(HTTP_TUNNEL_REFUSED_RESPONSE, peer_addr).await;
                            None
                        }
                        StreamMessage::NoClientTunnel => {
                            tracing::info!(remote = %host, sid = %stream_id, "client tunnel not found");
                            let _ = socket.send_to(HTTP_NOT_FOUND_RESPONSE, peer_addr).await;
                            None
                        }
                    }
                } else {
                    None
                };
        
                let data = match result {
                    Some(data) => data,
                    None => {
                        tracing::debug!(remote = %host, sid = %stream_id, "done tunneling to sink");
                        break
                    }
                };
        
                let result = socket.send_to(&data, peer_addr).await;
        
                if let Some(error) = result.err() {
                    tracing::warn!(remote = %host, sid = %stream_id, "stream closed, disconnecting: {:?}", error);
                    break
                }
            }

            tracing::debug!(
                cid = %client_id, sid = %stream_id, "tunnel_to_stream closed"
            );
            active_streams.remove(&stream_id);
        });

        ActiveStream {
            id: stream_id,
            client,
            tx,
            is_disabled: false,
        }
    }

    pub async fn send_to_remote(&mut self, message: StreamMessage) -> Result<(), SendError> {
        self.tx.send(message).await
    }

    pub async fn send_to_client(&mut self, packet: ControlPacket) -> Result<(), SendError> {
        self.client.send_to_client(packet).await
    }
    
    pub fn client_id(&self) -> ClientId {
        self.client.id
    }

    pub fn disable(&mut self) {
        self.is_disabled = true;
        self.close_channel()
    }

    pub fn is_disabled(&self) -> bool {
        self.is_disabled
    }
    
    pub fn close_channel(&self) {
        self.tx.close_channel()
    }
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    pub async fn send_to_client_init(&mut self) -> Result<(), SendError> {
        self.send_to_client(ControlPacket::Init(self.id)).await
    }

    pub async fn receive_from_remote_tcp<T>(
        &mut self,
        mut tcp_stream: T,
    ) -> Result<(), ReceiveFromRemoteTCPError> where
        T: AsyncRead + Unpin,
    {
        // now read from stream and forward to clients
        let mut buf = [0; 1024];

        // client is no longer connected
        // if Connections::get(conn, &tunnel_stream.client_id()).is_none() {
        if self.client.is_disabled() {
            tracing::debug!(cid = %self.client.id, sid = %self.id, "client is disabled, closing stream");

            // let _ = self.send_to_remote(StreamMessage::NoClientTunnel).await;
            return Err(ReceiveFromRemoteTCPError::ClientDisabled)
        }
        if self.is_disabled() {
            tracing::debug!(cid = %self.client.id, sid = %self.id, "active stream is disabled, closing stream");
            return Err(ReceiveFromRemoteTCPError::ActiveStreamDisabled)
        }

        // read from stream
        let n = match tcp_stream.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(cid = %self.client.id, sid = %self.id, "failed to read from tcp socket: {:?}", e);

                return Err(ReceiveFromRemoteTCPError::RemoteError);
            }
        };

        if n == 0 {
            tracing::debug!(cid = %self.client.id, sid = %self.id, "remote client streams end");
            let _ = self
                .send_to_client(ControlPacket::End(self.id))
                .await
                .map_err(|e| {
                    tracing::error!(cid = %self.client.id, sid = %self.id, "failed to send end signal: {:?}", e);
                });
            return Err(ReceiveFromRemoteTCPError::RemoteEnd);
        }

        tracing::debug!(cid = %self.client.id, sid = %self.id, "read {} bytes message from remote", n);
        let data = &buf[..n];
        let packet = ControlPacket::Data(self.id, data.to_vec());

        match self.send_to_client(packet).await {
            Ok(_) => {
                tracing::debug!(cid = %self.client.id, sid = %self.id, "sent data packet to client");
                Ok(())
            },
            Err(e) => {
                // This line extecuted when
                // - Corresponding client not found or closed
                tracing::warn!(cid = %self.client.id, sid = %self.id, "failed to forward tcp packets to disconnected client. dropping client. {:?}", e);
                Err(ReceiveFromRemoteTCPError::ClientError)
            }
        }
    }
}

#[derive(Error, Debug, PartialEq)]
pub enum ReceiveFromRemoteTCPError {
    #[error("Client is no longer available.")]
    ClientDisabled,
    #[error("Failed to send data to client.")]
    ClientError,
    #[error("ActiveStream is no longer available.")]
    ActiveStreamDisabled,
    #[error("Failed to send data to remote.")]
    RemoteError,
    #[error("Remote stream has closed.")]
    RemoteEnd,
}


#[derive(Debug, Clone, Default)]
pub struct ActiveStreams {
    streams: Arc<DashMap<StreamId, ActiveStream>>,
    addrs: Arc<DashMap<SocketAddr, StreamId>>
}

impl ActiveStreams {
    pub fn get(&self, stream_id: &StreamId) -> Option<Ref<StreamId, ActiveStream>> {
        self.streams.get(stream_id)
    }

    pub fn get_mut(&self, stream_id: &StreamId) -> Option<RefMut<StreamId, ActiveStream>> {
        self.streams.get_mut(stream_id)
    }

    pub fn insert(&self, stream_id: StreamId, active_stream: ActiveStream, addr: SocketAddr) -> Option<ActiveStream> {
        self.addrs.insert(addr, stream_id);
        self.streams.insert(stream_id, active_stream)
    }

    pub fn remove(&self, stream_id: &StreamId) -> Option<(StreamId, ActiveStream)> {
        // TODO: remove from addrs
        self.streams.remove(stream_id)
    }

    pub fn len(&self) -> usize {
        self.streams.len()
    }

    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }

    pub fn clear(&self) {
        self.streams.clear()
    }

    pub fn iter(&self) -> Iter<'_, StreamId, ActiveStream>{
        self.streams.iter()
    }

    pub fn find_by_addr(&self, addr: &SocketAddr) -> Option<Ref<StreamId, ActiveStream>> {
        // NOTE: 異なる StreamID をもつエントリの addr が重複していると、 insert のときに古い
        // StreamID のほうは参照できなくなる
        // ただし addr はグローバルユニークなはずなので気にしないことにする
        self.addrs.get(addr).and_then(|stream_id| self.streams.get(&stream_id))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamMessage {
    Data(Vec<u8>),
    TunnelRefused,
    NoClientTunnel,
}


#[cfg(test)]
mod active_streams_test {
    use magic_tunnel_lib::ClientId;
    use super::*;

    #[test]
    fn test_insert() {
        let active_streams = ActiveStreams::default();

        let stream_id = StreamId::new();
        let client_id = ClientId::new();
        let addr = "127.0.0.1:12345".parse().unwrap();
        let (tx, _) = unbounded();
        let client = ConnectedClient::new(client_id, "host".to_string(), tx);
        let (active_stream, _) = ActiveStream::new(client);

        assert!(active_streams.insert(stream_id, active_stream, addr).is_none());


        let stream_id2 = StreamId::new();
        let client_id2 = ClientId::new();
        let (tx, _) = unbounded();
        let client = ConnectedClient::new(client_id2, "host".to_string(), tx);
        let (active_stream, _) = ActiveStream::new(client);
        assert!(active_streams.insert(stream_id2, active_stream, addr).is_none());
        assert_eq!(active_streams.find_by_addr(&addr).unwrap().client.id, client_id2);
        assert_eq!(active_streams.addrs.len(), 1);
        assert_eq!(active_streams.streams.len(), 2);

    }


    #[test]
    fn test_insert_v6() {
        let active_streams = ActiveStreams::default();

        let stream_id = StreamId::new();
        let client_id = ClientId::new();
        let addr = "[::ffff:127.0.0.1]:64977".parse().unwrap();
        let (tx, _) = unbounded();
        let client = ConnectedClient::new(client_id, "host".to_string(), tx);
        let (active_stream, _) = ActiveStream::new(client);

        assert!(active_streams.insert(stream_id, active_stream, addr).is_none());


        let stream_id2 = StreamId::new();
        let client_id2 = ClientId::new();
        let (tx, _) = unbounded();
        let client = ConnectedClient::new(client_id2, "host".to_string(), tx);
        let (active_stream, _) = ActiveStream::new(client);
        assert!(active_streams.insert(stream_id2, active_stream, addr).is_none());
        assert_eq!(active_streams.find_by_addr(&addr).unwrap().client.id, client_id2);
        assert_eq!(active_streams.addrs.len(), 1);
        assert_eq!(active_streams.streams.len(), 2);

    }
 
}



#[cfg(test)]
mod active_streams_tcp_test {
    use super::*;
    use std::{io, time::Duration};
    use tokio_test::io::Builder;

    async fn wait() {
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    #[tokio::test]
    async fn mock_test() -> Result<(), Box<dyn std::error::Error>> {
        let mut tcp_mock = Builder::new().write(b"foobarbaz").write(b"piyo").build();

        // writed data must be exactly same as builder args
        // any grouping is accepted because of stream
        tcp_mock.write(b"foobar").await?;
        tcp_mock.write(b"bazpiyo").await?;
        Ok(())
    }
    #[tokio::test]
    async fn must_exit_from_loop_when_tcp_raises_error() -> Result<(), Box<dyn std::error::Error>> {
        let error = io::Error::new(io::ErrorKind::Other, "cruel");
        let tcp_mock = Builder::new().write_error(error).build();
        let client = ConnectedClient::default();
        let mut active_stream = ActiveStream::build_tcp(client, tcp_mock, ActiveStreams::default());

        active_stream.send_to_remote(StreamMessage::Data(b"foobar".to_vec())).await?;
        wait().await;

        // assert_eq!(reason, TunnelToStreamExitReason::TcpClosed);
        assert!(active_stream.is_closed());
        Ok(())
    }

    #[tokio::test]
    async fn tcp_stream_must_shutdown_when_tunnel_refused() -> Result<(), Box<dyn std::error::Error>>
    {
        let tcp_mock = Builder::new().build();
        let client = ConnectedClient::default();
        let mut active_stream = ActiveStream::build_tcp(client, tcp_mock, ActiveStreams::default());

        active_stream.send_to_remote(StreamMessage::TunnelRefused).await?;
        wait().await;

        // assert_eq!(reason, TunnelToStreamExitReason::QueueClosed);
        assert!(active_stream.is_closed());
        Ok(())
    }

    #[tokio::test]
    async fn tcp_stream_must_shutdown_when_no_client_tunnel(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let tcp_mock = Builder::new().build();
        let client = ConnectedClient::default();
        let mut active_stream = ActiveStream::build_tcp(client, tcp_mock, ActiveStreams::default());

        active_stream.send_to_remote(StreamMessage::NoClientTunnel).await?;
        wait().await;

        // assert_eq!(reason, TunnelToStreamExitReason::QueueClosed);
        assert!(active_stream.is_closed());
        Ok(())
    }

    #[tokio::test]
    async fn forward_data() -> Result<(), Box<dyn std::error::Error>> {
        let tcp_mock = Builder::new().write(b"foobarbaz").build();
        let client = ConnectedClient::default();
        let mut active_stream = ActiveStream::build_tcp(client, tcp_mock, ActiveStreams::default());

        active_stream.send_to_remote(StreamMessage::Data(b"foobarbaz".to_vec())).await?;
        active_stream.close_channel();
        wait().await;

        // assert_eq!(reason, TunnelToStreamExitReason::QueueClosed);
        Ok(())
    }
}



#[cfg(test)]
mod active_streams_receive_tcp_test {
    use super::*;
    use futures::channel::mpsc::{unbounded, UnboundedReceiver};
    use tokio_test::io::Builder;

    #[tokio::test]
    async fn send_stream_end() -> Result<(), Box<dyn std::error::Error>> {
        let mut tcp_mock = Builder::new().build();
        let (tx, mut client_rx) = unbounded();
        let client = ConnectedClient::new(ClientId::new(), "foohost".into(), tx);
        let (mut active_stream, _) = ActiveStream::new(client);
        let stream_id = active_stream.id;

        assert_eq!(active_stream.receive_from_remote_tcp(&mut tcp_mock).await, Err(ReceiveFromRemoteTCPError::RemoteEnd));

        assert_eq!(
            client_rx.next().await.unwrap(),
            ControlPacket::End(stream_id)
        );

        Ok(())
    }

    #[tokio::test]
    async fn forward_data() -> Result<(), Box<dyn std::error::Error>> {
        let mut tcp_mock = Builder::new().read(b"foobar").build();
        let (tx, mut client_rx) = unbounded();
        let client = ConnectedClient::new(ClientId::new(), "foohost".into(), tx);
        let (mut active_stream, _) = ActiveStream::new(client);
        let stream_id = active_stream.id;


        assert_eq!(active_stream.receive_from_remote_tcp(&mut tcp_mock).await, Ok(()));
        assert_eq!(
            client_rx.next().await.unwrap(),
            ControlPacket::Data(stream_id, b"foobar".to_vec())
        );

        assert_eq!(active_stream.receive_from_remote_tcp(&mut tcp_mock).await, Err(ReceiveFromRemoteTCPError::RemoteEnd));

        assert_eq!(
            client_rx.next().await.unwrap(),
            ControlPacket::End(stream_id)
        );
        Ok(())
    }

    // TODO: cancellation
    // #[tokio::test]
    // async fn stop_wait_read_when_tunnel_stream_is_closed() -> Result<(), Box<dyn std::error::Error>>
    // {
    //     let (conn, client, active_stream, _stream_rx, mut client_rx) = create_active_stream();
    //     Connections::add(&conn, client);

    //     client_rx.close();
    //     let infinite_reader = InfiniteRead::new();
    //     let _ = process_tcp_stream(&conn, active_stream, infinite_reader).await;

    //     Ok(())
    // }
}