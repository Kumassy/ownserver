use chrono::Duration;
use futures::{channel::mpsc::SendError, io};
use ownserver_lib::{ClientId, StreamId};
use thiserror::Error;

pub mod client;
pub use client::Client;
pub mod control_server_v2;
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
    pub metrics_idle_timeout: u64,
    pub remote_port_start: u16,
    pub remote_port_end: u16,
    pub periodic_cleanup_interval: u64,
    pub periodic_ping_interval: u64,
    pub reconnect_window: Duration,
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

#[derive(Error, Debug )]
pub enum SendToRemoteError {
    #[error("Failed to send data to remote.")]
    RemoteError(String),
    #[error("Stream {0} is not registered to Store or no longer available.")]
    StreamNotAvailable(StreamId),
    #[error("Failed to write data to remote stream {0}.")]
    WriteError(#[from] io::Error),
    #[error("Remote stream has closed.")]
    RemoteEnd,
}


#[derive(Error, Debug)]
pub enum SendToClientError {
    #[error("Failed to encode data {0}.")]
    EncodeError(#[from] io::Error),
    #[error("Failed to write data to client {0}.")]
    WriteError(#[from] warp::Error),
    #[error("Client {0} is not registered to Store or no longer available.")]
    ClientNotFound(ClientId),
    #[error("Client {0} is registered but waiting for cleanup.")]
    ClientNotConnected(ClientId),
}
