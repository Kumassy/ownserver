use dashmap::DashMap;
use dashmap::mapref::one::{Ref, RefMut};
use futures::channel::mpsc::UnboundedSender;
use ownserver_lib::{StreamId, EndpointId, Endpoint, ClientId, Endpoints};
use tokio::net::ToSocketAddrs;

#[derive(Debug, Clone)]
pub enum StreamMessage {
    Data(Vec<u8>),
    Close,
}
pub mod error;
pub mod local;
pub mod localudp;
pub mod proxy_client;
pub mod api;

pub type LocalStream = UnboundedSender<StreamMessage>;
#[derive(Debug, Default)]
pub struct Store {
    streams: DashMap<StreamId, LocalStream>,
    endpoints_map: DashMap<EndpointId, Endpoint>,
}

impl Store {
    pub fn add_stream(&self, stream_id: StreamId, stream: LocalStream) {
        self.streams.insert(stream_id, stream);
    }

    pub fn remove_stream(&self, stream_id: &StreamId) -> Option<(StreamId, LocalStream)> {
        self.streams.remove(stream_id)
    }

    pub fn has_stream(&self, stream_id: &StreamId) -> bool {
        self.streams.contains_key(stream_id)
    }

    pub fn get_stream(&self, stream_id: &StreamId) -> Option<Ref<StreamId, LocalStream>> {
        self.streams.get(stream_id)
    }

    pub fn get_mut_stream(&self, stream_id: &StreamId) -> Option<RefMut<StreamId, LocalStream>> {
        self.streams.get_mut(stream_id)
    }

    pub fn len_stream(&self) -> usize {
        self.streams.len()
    }

    pub fn list_streams(&self) -> Vec<StreamId> {
        self.streams.iter().map(|x| *x.key()).collect()
    }

    pub fn register_endpoints(&self, endpoints: Vec<Endpoint>) {
        for endpoint in endpoints {
            self.endpoints_map.insert(endpoint.id, endpoint);
        }
    }

    pub fn get_local_addr_by_endpoint_id(&self, eid: EndpointId) -> Option<impl ToSocketAddrs + std::fmt::Debug + Clone> {
        let endpoint = self.endpoints_map.get(&eid)?;

        Some(format!("localhost:{}", endpoint.local_port))
    }

    pub fn get_endpoint_by_endpoint_id(&self, eid: EndpointId) -> Option<Endpoint> {
        self.endpoints_map.get(&eid).map(|e| e.value().clone())
    }

    pub fn get_endpoints(&self) -> Endpoints {
        self.endpoints_map.iter().map(|e| e.value().clone()).collect()
    }

}
