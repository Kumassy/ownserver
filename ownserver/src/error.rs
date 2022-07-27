use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Failed to connect to control server: {0}.")]
    WebSocketError(#[from] tokio_tungstenite::tungstenite::error::Error),

    #[error("The server responded with an invalid response.")]
    ServerReplyInvalid,

    #[error("The server did not respond to our client_hello.")]
    NoResponseFromServer,

    #[error("Could not connect to server.")]
    ServerDown,

    #[error("Client hello was invalid or malformed.")]
    BadRequest,

    #[error("Server could not process client request.")]
    ServiceTemporaryUnavailable,

    #[error("Client tried to connect to a host that was not assigned by token server.")]
    IllegalHost,

    #[error("Server encountered some errors.")]
    InternalServerError,

    #[error("Current client handshake version is not supported.")]
    ClientHandshakeVersionMismatch,

    #[error("Server sent a malformed message.")]
    MalformedMessageFromServer,

    #[error("The server timed out sending us something.")]
    Timeout,

    #[error("Join error.")]
    JoinError(#[from] tokio::task::JoinError),
}
