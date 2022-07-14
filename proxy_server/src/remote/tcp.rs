use metrics::{increment_counter};
use std::io;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tracing::Instrument;
use tokio_util::sync::CancellationToken;

use crate::{RemoteTcp, Store, RemoteStream};
pub use magic_tunnel_lib::{ClientId, ControlPacket, StreamId};

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
