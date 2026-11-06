use shrew_proto::ApiError;
use shrew_proto::data::HandshakeError;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("invalid server address: {0}")]
    BadEndpoint(String),
    #[error("could not reach the server: {0}")]
    Io(#[from] std::io::Error),
    #[error("websocket: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("timed out")]
    Timeout,
    #[error("connection closed")]
    Closed,
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error(transparent)]
    Handshake(#[from] HandshakeError),
    #[error("unexpected result: {0}")]
    Decode(#[from] serde_json::Error),
}

impl ClientError {
    /// True when retrying the same thing later could work (network trouble).
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Io(_) | Self::WebSocket(_) | Self::Timeout | Self::Closed)
    }
    pub fn api_code(&self) -> Option<&str> {
        match self {
            Self::Api(e) => Some(&e.code),
            _ => None,
        }
    }
}
