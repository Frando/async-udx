use std::fmt;
use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdxError {
    kind: io::ErrorKind,
    reason: String,
}

impl fmt::Display for UdxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind_str = io::Error::from(self.kind);
        write!(f, "{} {}", kind_str, self.reason)
    }
}

impl std::error::Error for UdxError {}

impl UdxError {
    pub fn new(kind: io::ErrorKind, reason: impl ToString) -> Self {
        Self {
            kind,
            reason: reason.to_string(),
        }
    }

    pub fn close_graceful() -> Self {
        Self::new(io::ErrorKind::ConnectionReset, "")
    }

    pub fn closed_by_remote() -> Self {
        Self::new(io::ErrorKind::UnexpectedEof, "")
    }
}

impl From<io::Error> for UdxError {
    fn from(err: io::Error) -> Self {
        Self::new(err.kind(), format!("{}", err))
    }
}

impl From<&io::Error> for UdxError {
    fn from(err: &io::Error) -> Self {
        Self::new(err.kind(), format!("{}", err))
    }
}

impl From<UdxError> for io::Error {
    fn from(err: UdxError) -> Self {
        io::Error::new(err.kind, err.reason)
    }
}
