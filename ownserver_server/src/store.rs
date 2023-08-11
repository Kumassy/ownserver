use std::{net::{SocketAddr}, collections::{HashMap}, ops::Range};

use dashmap::DashMap;
use ownserver_lib::{StreamId, ClientId, ControlPacket, EndpointClaims, Endpoints, ControlPacketV2, EndpointId, Endpoint};
use metrics::gauge;
use rand::Rng;
use tokio::{sync::{RwLock, Mutex}, net::ToSocketAddrs};

use crate::{remote::stream::{RemoteStream, StreamMessage}, Client, ClientStreamError, port_allocator::{PortAllocator, PortAllocatorError}};


#[derive(Debug, Default)]
pub struct Store {
    streams: RwLock<HashMap<StreamId, RemoteStream>>,
    clients: RwLock<HashMap<ClientId, Client>>,
    addrs_map: DashMap<SocketAddr, StreamId>,
    endpoints_map: DashMap<EndpointId, Endpoint>,
    alloc: Mutex<PortAllocator>,
}

impl Store {
    pub fn new(range: Range<u16>) -> Self {
        Self {
            streams: Default::default(),
            clients: Default::default(),
            addrs_map: Default::default(),
            endpoints_map: Default::default(),
            alloc: Mutex::new(PortAllocator::new(range)),
        }
    }

    pub async fn send_to_client(&self, client_id: ClientId, packet: ControlPacketV2) -> Result<(), ClientStreamError> {
        match self.clients.write().await.get_mut(&client_id) {
            Some(client) => {
                client.send_to_client(packet).await
            },
            None => {
                Err(ClientStreamError::ClientNotAvailable(client_id))
            },
        }
    }

    pub async fn broadcast_to_clients(&self, packet: ControlPacketV2) {
        let client_ids = self.clients.read().await.keys().cloned().collect::<Vec<_>>();
        for client_id in client_ids {
            if let Err(e) = self.send_to_client(client_id, packet.clone()).await {
                tracing::warn!(cid = %client_id, "failed to send packet {:?}", e);
            }
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

    pub async fn disable_remote_by_client(&self, client_id: ClientId) {
        for (_, stream) in self.streams.write().await.iter_mut() {
            if stream.client_id() == client_id {
                stream.disable()
            }
        }
    }
    pub async fn disable_client(&self, client_id: ClientId) {
        if let Some(client) = self.clients.write().await.get_mut(&client_id) {
            client.disable().await;
        }
    }

    pub async fn add_client(&self, client: Client) {
        let client_id = client.client_id;
        self.clients.write().await.insert(client_id, client);

        let v = self.len_clients().await as f64;
        gauge!("ownserver_server.store.clients", v);
    }

    pub async fn add_remote(&self, remote: RemoteStream, peer_addr: SocketAddr) {
        let stream_id = remote.stream_id();
        self.streams.write().await.insert(stream_id, remote);
        self.addrs_map.insert(peer_addr, stream_id);

        let v = self.len_streams().await as f64;
        gauge!("ownserver_server.store.streams", v);
    }

    pub async fn cleanup(&self) {
        tracing::debug!("Store::cleanup");
        self.streams.write().await.retain(|_, v| !v.disabled());

        let mut eids_to_remove = Vec::new();
        for (_, client ) in self.clients.read().await.iter() {
            if client.disabled() {
                client.endpoints().iter().for_each(|e| {
                    eids_to_remove.push(e.id)
                });
            }
        }
        self.clients.write().await.retain(|_, v| !v.disabled());
        for eid in eids_to_remove {
            if let Err(e) = self.release_endpoint(eid).await {
                tracing::warn!(eid = %eid, "failed to release endpoint {:?}", e);
            }
        }

        let v = self.len_clients().await as f64;
        gauge!("ownserver_server.store.clients", v);
        let v = self.len_streams().await as f64;
        gauge!("ownserver_server.store.streams", v);
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

    pub async fn len_streams(&self) -> usize {
        self.streams.read().await.len()
    }

    pub async fn len_clients(&self) -> usize {
        self.clients.read().await.len()
    }


    pub async fn allocate_port(&self, rng: &mut impl Rng) -> Result<u16, PortAllocatorError> {
        self.alloc.lock().await.allocate_port(rng)
    }

    pub async fn allocate_endpoints(&self, rng: &mut impl Rng, client_claims: EndpointClaims) -> Result<Endpoints, PortAllocatorError> {
        let endpoints = self.alloc.lock().await.allocate_ports(rng, client_claims)?;
        for endpoint in endpoints.clone().into_iter() {
            self.endpoints_map.insert(endpoint.id, endpoint);
        }
        Ok(endpoints)
    }

    pub async fn release_endpoint(&self, eid: EndpointId) -> Result<(), PortAllocatorError> {
        let endpoint = self.endpoints_map.get(&eid).ok_or(PortAllocatorError::PortOutOfRange)?;
        self.alloc.lock().await.release_port(endpoint.remote_port)
    }

    pub fn get_remote_addr_by_endpoint_id(&self, eid: EndpointId) -> Option<impl ToSocketAddrs + std::fmt::Debug + Clone> {
        let endpoint = self.endpoints_map.get(&eid)?;

        Some(format!("0.0.0.0:{}", endpoint.remote_port))
    }

}