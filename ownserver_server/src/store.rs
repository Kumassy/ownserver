use std::{net::SocketAddr, collections::HashMap};

use dashmap::DashMap;
use ownserver_lib::{StreamId, ClientId, ControlPacket};
use metrics::gauge;
use tokio::sync::RwLock;

use crate::{remote::stream::{RemoteStream, StreamMessage}, Client, ClientStreamError};


#[derive(Debug, Default)]
pub struct Store {
    streams: RwLock<HashMap<StreamId, RemoteStream>>,
    clients: RwLock<HashMap<ClientId, Client>>,
    addrs_map: DashMap<SocketAddr, StreamId>,
    hosts_map: DashMap<String, ClientId>,
}

impl Store {
    pub async fn send_to_client(&self, client_id: ClientId, packet: ControlPacket) -> Result<(), ClientStreamError> {
        match self.clients.write().await.get_mut(&client_id) {
            Some(client) => {
                client.send_to_client(packet).await
            },
            None => {
                Err(ClientStreamError::ClientNotAvailable(client_id))
            },
        }
    }

    pub async fn send_to_remote(&self, stream_id: StreamId, message: StreamMessage) -> Result<(), ClientStreamError> {
        match self.streams.write().await.get_mut(&stream_id) {
            Some(stream) => {
                stream.send_to_remote(stream_id, message).await
            },
            None => {
                Err(ClientStreamError::StreamNotAvailable(stream_id))
            },
        }
    }

    pub async fn disable_remote(&self, stream_id: StreamId) {
        if let Some(stream) = self.streams.write().await.get_mut(&stream_id) {
            stream.disable();
        }
    }

    pub async fn disable_client(&self, client_id: ClientId) {
        if let Some(client) = self.clients.write().await.get_mut(&client_id) {
            client.disable();
        }
    }

    pub async fn add_client(&self, client: Client) {
        let client_id = client.client_id;
        let host = client.host.clone();
        self.clients.write().await.insert(client_id, client);
        self.hosts_map.insert(host, client_id);

        let v = self.clients.read().await.len() as f64;
        gauge!("ownserver_server.store.clients", v);
    }

    pub async fn add_remote(&self, remote: RemoteStream, peer_addr: SocketAddr) {
        let stream_id = remote.stream_id();
        self.streams.write().await.insert(stream_id, remote);
        self.addrs_map.insert(peer_addr, stream_id);
        let v = self.streams.read().await.len() as f64;
        gauge!("ownserver_server.store.streams", v);
    }

    pub async fn cleanup(&self) {
        self.streams.write().await.retain(|_, v| !v.disabled());
        self.clients.write().await.retain(|_, v| !v.disabled());
    }

    pub async fn find_stream_id_by_addr(&self, addr: &SocketAddr) -> Option<StreamId> {
        let stream_id = if let Some(e) = self.addrs_map.get(addr) {
            e.value().to_owned()
        } else {
            return None
        };
        
        if let Some(stream) = self.streams.read().await.get(&stream_id) {
            if !stream.disabled() {
                return Some(stream.stream_id())
            }
        }
        None
    }

    pub async fn get_stream_ids(&self) -> Vec<StreamId> {
        self.streams.read().await.iter().map(|(_, v)| v.stream_id()).collect()
    }
}