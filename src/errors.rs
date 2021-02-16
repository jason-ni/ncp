use thiserror::Error;

pub type Result<T> = core::result::Result<T, NcpError>;

#[allow(dead_code)]
#[derive(Error, Debug)]
pub enum NcpError {
    #[error("NCP error: {0}")]
    GeneralError(String),
    #[error("std io error: {0}")]
    StdIoError(#[from] std::io::Error),
    #[error("tokio mpsc error: {0}")]
    TokioMpscClosed(#[from] tokio::sync::mpsc::error::ClosedError),
    #[error("failed to parse addr: {0}")]
    AddrParseError(#[from] std::net::AddrParseError),
}
