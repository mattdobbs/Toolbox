//! Error type crossing the FFI boundary. Swift sees one enum with a message.

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum CoreError {
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("host key rejected: {0}")]
    HostKey(String),
    #[error("i/o error: {0}")]
    Io(String),
    #[error("storage error: {0}")]
    Store(String),
    #[error("invalid argument: {0}")]
    Invalid(String),
}

impl From<std::io::Error> for CoreError {
    fn from(e: std::io::Error) -> Self {
        CoreError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for CoreError {
    fn from(e: serde_json::Error) -> Self {
        CoreError::Store(e.to_string())
    }
}
