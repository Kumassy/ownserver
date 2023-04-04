use futures::{channel::mpsc::SendError};
use ownserver_lib::{ClientId, StreamId};
use thiserror::Error;

pub mod client;
pub use client::Client;
pub mod control_server;
pub mod remote;
pub mod proxy_server;
pub mod port_allocator;
pub mod store;
pub use store::Store;

#[derive(Debug, Clone)]
pub struct Config {
    pub control_port: u16,
    pub token_secret: String,
    pub host: String,
    pub remote_port_start: u16,
    pub remote_port_end: u16,
    pub periodic_cleanup_interval: u64,
    pub periodic_ping_interval: u64,
}


#[derive(Error, Debug, PartialEq)]
pub enum ProxyServerError {
    #[error("Failed to load config because it is not initialized.")]
    ConfigNotInitialized,
}

#[derive(Error, Debug, PartialEq)]
pub enum ForwardingError {
    #[error("Destination is disabled.")]
    DestinationDisabled,
    #[error("Failed to put data into sender buffer.")]
    SendError(#[from] SendError),
}

#[derive(Error, Debug, PartialEq)]
pub enum ClientStreamError {
    #[error("Failed to send data to client {0}.")]
    ClientError(String),
    #[error("Failed to send data to remote.")]
    RemoteError(String),
    #[error("Client {0} is not registered to Store or no longer available.")]
    ClientNotAvailable(ClientId),
    #[error("Stream {0} is not registered to Store or no longer available.")]
    StreamNotAvailable(StreamId),
    #[error("Remote stream has closed.")]
    RemoteEnd,
}

