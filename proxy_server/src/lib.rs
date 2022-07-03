use futures::channel::mpsc::SendError;
use thiserror::Error;

pub mod active_stream;
pub mod connected_clients;
pub mod control_server;
pub mod remote;
pub mod proxy_server;
pub mod port_allocator;

#[derive(Debug, Clone)]
pub struct Config {
    pub control_port: u16,
    pub token_secret: String,
    pub host: String,
    pub remote_port_start: u16,
    pub remote_port_end: u16,
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
