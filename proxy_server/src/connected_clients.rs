use dashmap::DashMap;
use futures::{Sink, StreamExt, SinkExt};
use futures::channel::mpsc::{UnboundedSender, unbounded, UnboundedReceiver};
pub use magic_tunnel_lib::{ClientId, ControlPacket};
use metrics::gauge;
use warp::ws::Message;
use std::fmt::Formatter;
use std::sync::Arc;

#[derive(Clone)]
pub struct ConnectedClient {
    pub id: ClientId,
    pub host: String,
    // pub is_anonymous: bool,
    pub tx: UnboundedSender<ControlPacket>,
}

impl std::fmt::Debug for ConnectedClient {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectedClient")
            .field("id", &self.id)
            .field("sub", &self.host)
            // .field("anon", &self.is_anonymous)
            .finish()
    }
}

impl ConnectedClient {
    pub fn new<T>(id: ClientId, host: String, mut sink: T) -> Self where T: Sink<Message> + Unpin + std::marker::Send + 'static, T::Error: std::fmt::Debug{
        let (tx, mut rx) = unbounded::<ControlPacket>();

        tokio::spawn(async move {
            loop {
                match rx.next().await {
                    Some(packet) => {
                        let data = match rmp_serde::to_vec(&packet) {
                            Ok(data) => data,
                            Err(error) => {
                                tracing::warn!(cid = %id, error = ?error, "failed to encode message");
                                // return client;
                                break
                            }
                        };
        
                        let result = sink.send(Message::binary(data)).await;
                        if let Err(error) = result {
                            tracing::debug!(cid = %id, error = ?error, "client disconnected: aborting");
                            // return client;
                            break
                        }
                    }
                    None => {
                        tracing::debug!(cid = %id, "ending client tunnel");
                        // return client;
                        break
                    }
                };
            }

            // TODO: some cleanup code
            // cancel client code
        });

        ConnectedClient { id, host, tx }
    }

    pub fn forward_packet_to_client() -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }

    pub fn register_sink() {
        
    }
}

#[derive(Debug)]
pub struct Connections {
    clients: Arc<DashMap<ClientId, ConnectedClient>>,
    hosts: Arc<DashMap<String, ConnectedClient>>,
}

impl Default for Connections {
    fn default() -> Self {
        Self {
            clients: Arc::new(DashMap::new()),
            hosts: Arc::new(DashMap::new()), 
        }
    }
}

impl Connections {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(DashMap::new()),
            hosts: Arc::new(DashMap::new()),
        }
    }

    // pub fn update_host(connection: &mut Self, client: &ConnectedClient) {
    //     connection
    //         .hosts
    //         .insert(client.host.clone(), client.clone());
    // }

    pub fn remove(&self, client: &ConnectedClient) {
        client.tx.close_channel();

        // https://github.com/agrinman/tunnelto/blob/0.1.9/src/server/connected_clients.rs
        self.hosts.remove(&client.host);
        self.clients.remove(&client.id);
        tracing::debug!(cid = %client.id, "rm client");
        gauge!("magic_tunnel_server.control.connections", self.clients.len() as f64);
    }

    // pub fn client_for_host(connection: &mut Self, host: &String) -> Option<ClientId> {
    //     connection.hosts.get(host).map(|c| c.id.clone())
    // }

    pub fn get(&self, client_id: &ClientId) -> Option<ConnectedClient> {
        self
            .clients
            .get(client_id)
            .map(|c| c.value().clone())
    }

    pub fn find_by_host(&self, host: &String) -> Option<ConnectedClient> {
        self.hosts.get(host).map(|c| c.value().clone())
    }

    pub fn add(&self, client: ConnectedClient) {
        self.clients.insert(client.id, client.clone());
        self.hosts.insert(client.host.clone(), client);
        gauge!("magic_tunnel_server.control.connections", self.clients.len() as f64);
    }

    pub fn clear(&self) {
        self.clients.clear();
        self.hosts.clear();
    }

    pub fn len_clients(&self) -> usize {
        self.clients.len()
    }

    pub fn len_hosts(&self) -> usize {
        self.hosts.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::mpsc::unbounded;

    fn setup() -> (Connections, ConnectedClient, ClientId) {
        let conn = Connections::new();
        let (tx, _rx) = unbounded::<ControlPacket>();
        let client_id = ClientId::new();
        let client = ConnectedClient {
            id: client_id,
            host: "foobar".into(),
            tx,
        };

        (conn, client, client_id)
    }

    #[test]
    fn connections_clients_should_be_empty() {
        let (conn, _, client_id) = setup();

        assert!(conn.get(&client_id).is_none());
    }

    #[test]
    fn connections_hosts_should_be_empty() {
        let (conn, _, _) = setup();

        assert!(conn.find_by_host(&"foobar".to_owned()).is_none());
    }

    #[test]
    fn connections_client_should_be_registered() {
        let (conn, client, client_id) = setup();

        conn.add(client);

        assert_eq!(conn.get(&client_id).unwrap().id, client_id);
    }

    #[test]
    fn connections_hosts_should_be_registered() {
        let (conn, client, client_id) = setup();

        conn.add(client);

        assert_eq!(
            conn.find_by_host(&"foobar".to_owned())
                .unwrap()
                .id,
            client_id
        );
    }

    #[test]
    fn connections_client_should_be_empty_after_remove() {
        let (conn, client, client_id) = setup();

        conn.add(client.clone());
        conn.remove(&client);

        assert!(conn.get(&client_id).is_none());
    }

    #[test]
    fn connections_hosts_should_be_empty_after_remove() {
        let (conn, client, _) = setup();

        conn.add(client.clone());
        conn.remove(&client);

        assert!(conn.find_by_host(&"foobar".to_owned()).is_none());
    }

    #[test]
    fn connections_hosts_ignore_multiple_add() {
        let (conn, client, _) = setup();

        conn.add(client.clone());
        conn.add(client.clone());
        conn.add(client.clone());

        assert_eq!(conn.clients.len(), 1);
        assert_eq!(conn.hosts.len(), 1);
    }

    #[test]
    fn connections_hosts_ignore_multiple_remove() {
        let (conn, client, _) = setup();

        conn.add(client.clone());
        conn.remove(&client);
        conn.remove(&client);
        conn.remove(&client);

        assert_eq!(conn.clients.len(), 0);
        assert_eq!(conn.hosts.len(), 0);
    }

    #[test]
    fn connections_should_clear() {
        let (conn, client, _) = setup();

        conn.add(client);
        conn.clear();

        assert_eq!(conn.clients.len(), 0);
        assert_eq!(conn.hosts.len(), 0);
    }
}
