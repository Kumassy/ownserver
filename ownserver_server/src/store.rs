use std::net::SocketAddr;

use dashmap::DashMap;
use ownserver_lib::{StreamId, ClientId, ControlPacket};
use metrics::gauge;

use crate::{remote::stream::{RemoteStream, StreamMessage}, Client, ClientStreamError};


#[derive(Debug, Default)]
pub struct Store {
    streams: DashMap<StreamId, RemoteStream>,
    addrs_map: DashMap<SocketAddr, StreamId>,

    clients: DashMap<ClientId, Client>,
    hosts_map: DashMap<String, ClientId>,
}

impl Store {
    pub async fn send_to_client(&self, client_id: ClientId, packet: ControlPacket) -> Result<(), ClientStreamError> {
        match self.clients.get_mut(&client_id) {
            Some(mut client) => {
                client.send_to_client(packet).await
            },
            None => {
                Err(ClientStreamError::ClientNotAvailable(client_id))
            }
        }
    }

    pub async fn send_to_remote(&self, stream_id: StreamId, message: StreamMessage) -> Result<(), ClientStreamError> {
        match self.streams.get_mut(&stream_id) {
            Some(mut stream) => {
                stream.send_to_remote(stream_id, message).await
            },
            None => {
                Err(ClientStreamError::StreamNotAvailable(stream_id))
            }
        }
    }

    pub fn disable_remote(&self, stream_id: StreamId) {
        if let Some(mut stream) = self.streams.get_mut(&stream_id) {
            stream.disable();
        }
    }

    pub fn disable_client(&self, client_id: ClientId) {
        if let Some(mut client) = self.clients.get_mut(&client_id) {
            client.disable();
        }
    }

    pub fn add_client(&self, client: Client) {
        let client_id = client.client_id;
        let host = client.host.clone();
        self.clients.insert(client_id, client);
        self.hosts_map.insert(host, client_id);
        gauge!("ownserver_server.store.clients", self.clients.len() as f64);
    }

    pub fn add_remote(&self, remote: RemoteStream, peer_addr: SocketAddr) {
        let stream_id = remote.stream_id();
        self.streams.insert(stream_id, remote);
        self.addrs_map.insert(peer_addr, stream_id);
        gauge!("ownserver_server.store.streams", self.streams.len() as f64);
    }

    pub fn cleanup(&self) {
        self.streams.retain(|_, v| !v.disabled());
        self.clients.retain(|_, v| !v.disabled());
    }

    pub fn find_stream_id_by_addr(&self, addr: &SocketAddr) -> Option<StreamId> {
        if let Some(stream) = self.addrs_map.get(addr).and_then(|stream_id| self.streams.get(&stream_id)) {
            if !stream.disabled() {
                return Some(stream.stream_id())
            }
        }
        None
    }

    pub fn get_stream_ids(&self) -> Vec<StreamId> {
        self.streams.iter().map(|v| v.stream_id()).collect()
    }
}