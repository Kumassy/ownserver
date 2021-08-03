pub use magic_tunnel_lib::{ClientId, ControlPacket};
use dashmap::DashMap;
use std::fmt::Formatter;
use futures::channel::mpsc::UnboundedSender;
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

#[derive(Debug)]
pub struct Connections {
    clients: Arc<DashMap<ClientId, ConnectedClient>>,
    hosts: Arc<DashMap<String, ConnectedClient>>,
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

    pub fn remove(connection: &Self, client: &ConnectedClient) {
        client.tx.close_channel();

        // https://github.com/agrinman/tunnelto/blob/0.1.9/src/server/connected_clients.rs
        connection.hosts.remove(&client.host);
        connection.clients.remove(&client.id);
        tracing::debug!("rm client: {}", &client.id);
    }

    // pub fn client_for_host(connection: &mut Self, host: &String) -> Option<ClientId> {
    //     connection.hosts.get(host).map(|c| c.id.clone())
    // }

    pub fn get(connection: &Self, client_id: &ClientId) -> Option<ConnectedClient> {
        connection
            .clients
            .get(&client_id)
            .map(|c| c.value().clone())
    }

    pub fn find_by_host(connection: &Self, host: &String) -> Option<ConnectedClient> {
        connection.hosts.get(host).map(|c| c.value().clone())
    }

    pub fn add(connection: &Self, client: ConnectedClient) {
        connection
            .clients
            .insert(client.id.clone(), client.clone());
        connection.hosts.insert(client.host.clone(), client);
    }

    pub fn clear(connection: &Self) {
        connection
            .clients
            .clear();
        connection
            .hosts
            .clear();
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::mpsc::unbounded;

    fn setup() -> (Connections, ConnectedClient, ClientId) {
        let conn = Connections::new();
        let (tx, _rx) = unbounded::<ControlPacket>();
        let client_id = ClientId::generate();
        let client = ConnectedClient {
            id: client_id.clone(),
            host: "foobar".into(),
            tx
        };

        (conn, client, client_id)
    }

    #[test]
    fn connections_clients_should_be_empty() {
        let (conn, _, client_id) = setup();

        assert!(Connections::get(&conn, &client_id).is_none());
    }

    #[test]
    fn connections_hosts_should_be_empty() {
        let (conn, _, _) = setup();

        assert!(Connections::find_by_host(&conn, &"foobar".to_owned()).is_none());
    }

    #[test]
    fn connections_client_should_be_registered() {
        let (conn, client, client_id) = setup();

        Connections::add(&conn, client);

        assert_eq!(Connections::get(&conn, &client_id).unwrap().id, client_id);
    }

    #[test]
    fn connections_hosts_should_be_registered() {
        let (conn, client, client_id) = setup();

        Connections::add(&conn, client);

        assert_eq!(Connections::find_by_host(&conn, &"foobar".to_owned()).unwrap().id, client_id);
    }

    #[test]
    fn connections_client_should_be_empty_after_remove() {
        let (conn, client, client_id) = setup();

        Connections::add(&conn, client.clone());
        Connections::remove(&conn, &client);

        assert!(Connections::get(&conn, &client_id).is_none());
    }

    #[test]
    fn connections_hosts_should_be_empty_after_remove() {
        let (conn, client, _) = setup();

        Connections::add(&conn, client.clone());
        Connections::remove(&conn, &client);

        assert!(Connections::find_by_host(&conn, &"foobar".to_owned()).is_none());
    }

    #[test]
    fn connections_hosts_ignore_multiple_add() {
        let (conn, client, _) = setup();

        Connections::add(&conn, client.clone());
        Connections::add(&conn, client.clone());
        Connections::add(&conn, client.clone());

        assert_eq!(conn.clients.len(), 1);
        assert_eq!(conn.hosts.len(), 1);
    }

    #[test]
    fn connections_hosts_ignore_multiple_remove() {
        let (conn, client, _) = setup();

        Connections::add(&conn, client.clone());
        Connections::remove(&conn, &client);
        Connections::remove(&conn, &client);
        Connections::remove(&conn, &client);
        
        assert_eq!(conn.clients.len(), 0);
        assert_eq!(conn.hosts.len(), 0);
    }

    #[test]
    fn connections_should_clear() {
        let (conn, client, _) = setup();

        Connections::add(&conn, client.clone());
        Connections::clear(&conn);
        
        assert_eq!(conn.clients.len(), 0);
        assert_eq!(conn.hosts.len(), 0);
    }
}