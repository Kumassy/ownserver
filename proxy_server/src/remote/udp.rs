use futures::channel::mpsc::UnboundedReceiver;
use futures::prelude::*;
use metrics::{increment_counter, gauge};
use std::io;
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use tokio::sync::oneshot;
use tracing::Instrument;
use tokio_util::sync::CancellationToken;
use std::sync::Arc;

use crate::active_stream::{ActiveStream, ActiveStreams, StreamMessage};
use crate::connected_clients::Connections;
use crate::control_server;
pub use magic_tunnel_lib::{ClientId, ControlPacket, StreamId};

// #[tracing::instrument(skip(conn, active_streams, listen_addr))]
pub async fn spawn_remote(
    conn: &'static Connections,
    active_streams: &'static ActiveStreams,
    listen_addr: impl ToSocketAddrs + std::fmt::Debug + Clone,
    host: String,
) -> io::Result<CancellationToken> {
    let socket = UdpSocket::bind(listen_addr.clone()).await?;
    tracing::info!(remote = %host, "remote process listening on {:?}", listen_addr);
    let socket = Arc::new(socket);

    let cancellation_token = CancellationToken::new();
    let ct = cancellation_token.clone();

    // read from socket, write to client
    let active_streams_clone = active_streams.clone();

    tokio::spawn(
        async move {
            let active_streams = active_streams_clone;
            process_udp_stream(ct, conn, active_streams, host, socket).await;
        }
        .instrument(tracing::info_span!("process_udp_stream")),
    );

    Ok(cancellation_token)
}

pub const HTTP_ERROR_LOCATING_HOST_RESPONSE: &'static [u8] =
    b"HTTP/1.1 500\r\nContent-Length: 27\r\n\r\nError: Error finding tunnel";
pub const HTTP_NOT_FOUND_RESPONSE: &'static [u8] =
    b"HTTP/1.1 404\r\nContent-Length: 23\r\n\r\nError: Tunnel Not Found";
pub const HTTP_TUNNEL_REFUSED_RESPONSE: &'static [u8] =
    b"HTTP/1.1 500\r\nContent-Length: 32\r\n\r\nTunnel says: connection refused.";

#[tracing::instrument(skip(ct, conn, active_streams, udp_socket))]
async fn process_udp_stream(
    ct: CancellationToken,
    conn: &Connections,
    mut active_streams: ActiveStreams,
    host: String,
    udp_socket: Arc<UdpSocket>,
)
{
    let mut buf = [0; 2048];

    // find the client listening for this host
    let client_id = match Connections::find_by_host(conn, &host) {
        Some(client) => client.id,
        None => {
            tracing::error!(remote = %host, "failed to find instance");
            return;
        }
    };

    loop {
        // client is no longer connected
        if Connections::get(conn, &client_id).is_none() {
            tracing::debug!(cid = %client_id, "client disconnected, closing stream");
            return;
        }

        // TODO: cancellation token
        // read from stream
        let (n, peer_addr) = match udp_socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(cid = %client_id, "failed to read from tcp socket: {:?}", e);
                return;
            }
        };

        let mut active_stream = match active_streams.find_by_addr(&peer_addr) {
            Some(active_stream) => active_stream.clone(),
            None => {
                let client = match Connections::get(conn, &client_id) {
                    Some(client) => client.clone(),
                    None => {
                        tracing::error!(client_id = %client_id, "failed to find client");
                        return;
                    }
                };

                let active_stream = ActiveStream::build_udp(client, udp_socket.clone(), peer_addr, active_streams.clone());
                let stream_id = active_stream.id;

                active_streams.insert(stream_id, active_stream.clone(), peer_addr);
                active_stream
            }
        };
        gauge!("magic_tunnel_server.remotes.udp.streams", active_streams.len() as f64);
        let stream_id = active_stream.id;

        if n == 0 {
            tracing::debug!(cid = %client_id, sid = %stream_id, "remote client streams end");
            let _ = active_stream
                .send_to_client(ControlPacket::End(stream_id))
                .await
                .map_err(|e| {
                    tracing::error!(cid = %client_id, sid = %stream_id, "failed to send end signal: {:?}", e);
                });
            return;
        }

        tracing::debug!(cid = %client_id, sid = %stream_id, "read {} bytes message from remote client", n);

        if active_stream.is_closed() {
            tracing::debug!(cid = %client_id, sid = %stream_id, "process_tcp_stream closed because active_stream.tx has closed");
            return;
        }

        let data = &buf[..n];
        let packet = ControlPacket::UdpData(stream_id, data.to_vec());

        match active_stream.send_to_client(packet.clone()).await {
            Ok(_) => tracing::debug!(cid = %client_id, sid = %stream_id, "sent data packet to client"),
            Err(_) => {
                // TODO: not tested
                // This line extecuted when
                // - Corresponding client not found or closed
                tracing::error!(cid = %client_id, sid = %stream_id, "failed to forward tcp packets to disconnected client. dropping client.");
                conn.remove_by_id(client_id);
                tracing::debug!(cid = %client_id, "remove client from connections len_clients={} len_hosts={}", Connections::len_clients(conn), Connections::len_hosts(conn));
            }
        }
    }
}