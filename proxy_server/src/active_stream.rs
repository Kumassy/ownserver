use crate::connected_clients::ConnectedClient;
use dashmap::{DashMap, iter::Iter, mapref::one::Ref};
use futures::{channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender}, stream};
use magic_tunnel_lib::StreamId;
use std::{sync::Arc, net::SocketAddr};

#[derive(Debug, Clone)]
pub struct ActiveStream {
    pub id: StreamId,
    pub client: ConnectedClient,
    pub tx: UnboundedSender<StreamMessage>,
}

impl ActiveStream {
    pub fn new(client: ConnectedClient) -> (Self, UnboundedReceiver<StreamMessage>) {
        let (tx, rx) = unbounded();
        (
            ActiveStream {
                id: StreamId::new(),
                client,
                tx,
            },
            rx,
        )
    }
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
        let client = ConnectedClient {
            id: client_id,
            host: "host".to_string(),
            tx,
        };
        let (active_stream, _) = ActiveStream::new(client);

        assert!(active_streams.insert(stream_id, active_stream, addr).is_none());


        let stream_id2 = StreamId::new();
        let client_id2 = ClientId::new();
        let (tx, _) = unbounded();
        let client = ConnectedClient {
            id: client_id2,
            host: "host".to_string(),
            tx,
        };
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
        let client = ConnectedClient {
            id: client_id,
            host: "host".to_string(),
            tx,
        };
        let (active_stream, _) = ActiveStream::new(client);

        assert!(active_streams.insert(stream_id, active_stream, addr).is_none());


        let stream_id2 = StreamId::new();
        let client_id2 = ClientId::new();
        let (tx, _) = unbounded();
        let client = ConnectedClient {
            id: client_id2,
            host: "host".to_string(),
            tx,
        };
        let (active_stream, _) = ActiveStream::new(client);
        assert!(active_streams.insert(stream_id2, active_stream, addr).is_none());
        assert_eq!(active_streams.find_by_addr(&addr).unwrap().client.id, client_id2);
        assert_eq!(active_streams.addrs.len(), 1);
        assert_eq!(active_streams.streams.len(), 2);

    }
 
}
