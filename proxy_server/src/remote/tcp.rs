use futures::channel::mpsc::UnboundedReceiver;
use futures::prelude::*;
use metrics::{increment_counter, gauge};
use std::io;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio::sync::oneshot;
use tracing::Instrument;
use tokio_util::sync::CancellationToken;

use crate::active_stream::{ActiveStream, ActiveStreams, StreamMessage};
use crate::connected_clients::Connections;
use crate::{control_server, RemoteTcp, Store, RemoteStream};
pub use magic_tunnel_lib::{ClientId, ControlPacket, StreamId};

#[tracing::instrument(skip(conn, active_streams, listen_addr))]
pub async fn spawn_remote(
    conn: &'static Connections,
    active_streams: &'static ActiveStreams,
    listen_addr: impl ToSocketAddrs + std::fmt::Debug + Clone,
    host: String,
) -> io::Result<CancellationToken> {
    // create our accept any server
    let listener = TcpListener::bind(listen_addr.clone()).await?;
    tracing::info!(remote = %host, "remote process listening on {:?}", listen_addr);

    let cancellation_token = CancellationToken::new();
    let ct = cancellation_token.clone();

    tokio::spawn(async move {
        loop {
            let socket = tokio::select! {
                socket = listener.accept() => {
                    match socket {
                        Ok((socket, _)) => socket,
                        _ => {
                            tracing::error!(remote = %host, "failed to accept socket");
                            continue;
                        }
                    }
                },
                _ = ct.cancelled() => {
                    tracing::info!(remote = %host, "tcp listener is cancelled.");
                    return;
                },
            };

            let host_clone = host.clone();
            tokio::spawn(
                async move {
                    accept_connection(conn, active_streams.clone(), socket, host_clone).await;
                }
                .instrument(tracing::info_span!("remote_connect")),
            );
        }
    }.instrument(tracing::info_span!("spawn_accept_connection")));
    Ok(cancellation_token)
}

// stream has selected because each stream listen different port
#[tracing::instrument(skip(conn, active_streams, socket))]
pub async fn accept_connection(
    conn: &'static Connections,
    active_streams: ActiveStreams,
    mut socket: TcpStream,
    host: String,
) {
    tracing::info!(remote = %host, "new remote connection");

    // find the client listening for this host
    let client = match Connections::find_by_host(conn, &host) {
        Some(client) => client.clone(),
        None => {
            tracing::error!(remote = %host, "failed to find instance");
            let _ = socket.write_all(HTTP_ERROR_LOCATING_HOST_RESPONSE).await;
            return;
        }
    };
    let peer_addr = match socket.peer_addr() {
        Ok(addr) => addr,
        Err(_) => {
            tracing::error!(remote = %host, "failed to find remote peer addr");
            let _ = socket.write_all(HTTP_TUNNEL_REFUSED_RESPONSE).await;
            return;
        }
    };
    increment_counter!("magic_tunnel_server.remotes.success");

    let (mut stream, sink) = tokio::io::split(socket);
    let client_id = client.id;

    // allocate a new stream for this request
    let mut active_stream = ActiveStream::build_tcp(client, sink, active_streams.clone());
    let stream_id = active_stream.id;

    tracing::info!(remote = %host, cid = %client_id, sid = %active_stream.id, "new stream connected");

    // add our stream
    active_streams.insert(stream_id, active_stream.clone(), peer_addr);
    gauge!("magic_tunnel_server.remotes.streams", active_streams.len() as f64);
    tracing::info!(remote = %host, cid = %client_id, sid = %active_stream.id, "register stream to active_streams len={}", active_streams.len());

    // read from socket, write to client
    let active_streams_clone = active_streams.clone();
    tokio::spawn(
        async move {
            let active_streams = active_streams_clone;

            if let Err(e) = active_stream.send_to_client_init().await {
                tracing::error!(cid = %client_id, sid = %stream_id, "failed to send stream init: {:?}", e);

                active_stream.disable();
                active_streams.remove(&stream_id);
                return;
            }
            tracing::debug!(cid = %client_id, sid = %stream_id, "send stream init");
            
            loop {
                if let Err(e) = active_stream.receive_from_remote_tcp(&mut stream).await {
                    tracing::info!(cid = %client_id, sid = %stream_id, "receive_from_remote_tcp returns error {:?}", e);
                    break;
                }
                // TODO: fix active_stream is clonable
                if active_streams.get(&stream_id).is_none() {
                    tracing::error!(cid = %client_id, sid = %stream_id, "receive_from_remote_tcp returns ok even when active_stream is disabled and removed");
                    break;
                }
            }
            active_stream.disable();
            active_streams.remove(&stream_id);
            gauge!("magic_tunnel_server.remotes.streams", active_streams.len() as f64);
            tracing::debug!(cid = %client_id, sid = %stream_id, "remove stream from active_streams, process_tcp_stream len={}", active_streams.len());
        }
        .instrument(tracing::info_span!("process_tcp_stream")),
    );

}

pub async fn spawn_remote2(
    store: Arc<Store>,
    listen_addr: impl ToSocketAddrs + std::fmt::Debug + Clone,
    client_id: ClientId,
    cancellation_token: CancellationToken,
) -> io::Result<()> {
    // create our accept any server
    let listener = TcpListener::bind(listen_addr.clone()).await?;
    tracing::info!(cid = %client_id, "remote process listening on {:?}", listen_addr);

    let ct = cancellation_token.clone();

    tokio::spawn(async move {
        loop {
            let socket = tokio::select! {
                socket = listener.accept() => {
                    match socket {
                        Ok((socket, _)) => socket,
                        _ => {
                            tracing::error!(cid = %client_id, "failed to accept socket");
                            continue;
                        }
                    }
                },
                _ = ct.cancelled() => {
                    tracing::info!(cid = %client_id, "tcp listener is cancelled.");
                    return;
                },
            };

            let store_ = store.clone();

            tokio::spawn(
                async move {
                    accept_connection2(store_, socket, client_id).await;
                }
                .instrument(tracing::info_span!("remote_connect")),
            );
        }
    }.instrument(tracing::info_span!("spawn_accept_connection")));
    Ok(())
}

pub async fn accept_connection2(
    store: Arc<Store>,
    socket: TcpStream,
    client_id: ClientId,
) {
    tracing::info!(cid = %client_id, "new remote connection");

    let peer_addr = match socket.peer_addr() {
        Ok(addr) => addr,
        Err(_) => {
            tracing::error!(cid = %client_id, "failed to find remote peer addr");
            // let _ = socket.write_all(HTTP_TUNNEL_REFUSED_RESPONSE).await;
            return;
        }
    };
    tracing::info!(cid = %client_id, "remote ip is {}", peer_addr);
    increment_counter!("magic_tunnel_server.remotes.success");


    let remote = RemoteTcp::new(store.clone(), socket, client_id);
    if remote.send_init_to_client().await.is_ok() {
        tracing::info!(cid = %client_id, sid = %remote.stream_id, "add new remote stream");
        store.add_remote(RemoteStream::RemoteTcp(remote), peer_addr);
    }
}


pub const HTTP_ERROR_LOCATING_HOST_RESPONSE: &'static [u8] =
    b"HTTP/1.1 500\r\nContent-Length: 27\r\n\r\nError: Error finding tunnel";
pub const HTTP_NOT_FOUND_RESPONSE: &'static [u8] =
    b"HTTP/1.1 404\r\nContent-Length: 23\r\n\r\nError: Tunnel Not Found";
pub const HTTP_TUNNEL_REFUSED_RESPONSE: &'static [u8] =
    b"HTTP/1.1 500\r\nContent-Length: 32\r\n\r\nTunnel says: connection refused.";


#[cfg(test)]
mod spawn_remote_test {
    use super::*;
    use crate::connected_clients::ConnectedClient;
    use dashmap::DashMap;
    use futures::channel::mpsc::unbounded;
    use lazy_static::lazy_static;
    use std::io;
    use std::sync::Arc;
    use std::time::Duration;

    async fn launch_remote(remote_port: u16) -> io::Result<CancellationToken> {
        lazy_static! {
            pub static ref CONNECTIONS: Connections = Connections::new();
            pub static ref ACTIVE_STREAMS: ActiveStreams = ActiveStreams::default();
        }
        // we must clear CONNECTIONS, ACTIVE_STREAMS
        // because they are shared across test
        Connections::clear(&CONNECTIONS);
        ACTIVE_STREAMS.clear();

        let (tx, _rx) = unbounded::<ControlPacket>();
        let client = ConnectedClient::new(ClientId::new(), "host-aaa".to_string(), tx);
        Connections::add(&CONNECTIONS, client.clone());

        spawn_remote(
            &CONNECTIONS,
            &ACTIVE_STREAMS,
            format!("[::]:{}", remote_port),
            "host-aaa".to_string(),
        )
        .await
    }

    #[tokio::test]
    async fn accept_remote_connection() -> Result<(), Box<dyn std::error::Error>> {
        let remote_port = 5678;
        let _remote_cancel_handler = launch_remote(remote_port).await?;

        let _ = TcpStream::connect(format!("127.0.0.1:{}", remote_port))
            .await
            .expect("Failed to connect to remote port");
        Ok(())
    }

    #[tokio::test]
    async fn reject_remote_connection_after_cancellation() -> Result<(), Box<dyn std::error::Error>>
    {
        let remote_port = 5679;
        let remote_cancel_handler = launch_remote(remote_port).await?;

        let _ = TcpStream::connect(format!("127.0.0.1:{}", remote_port))
            .await
            .expect("Failed to connect to remote port");
        remote_cancel_handler.cancel();
        tokio::time::sleep(Duration::from_secs(3)).await;

        let remote = TcpStream::connect(format!("127.0.0.1:{}", remote_port)).await;
        assert!(remote.is_err());

        Ok(())
    }
}
