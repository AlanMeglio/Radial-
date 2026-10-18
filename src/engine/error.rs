//! Typed errors for the engine. The UI can match on the variant to pick a message.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// A filesystem operation failed.
    Io(String),
    /// The input is damaged or not the format its name claims.
    Corrupt(String),
    /// The conversion or action does not exist for this input.
    Unsupported(String),
    /// The feature needs an OS codec that is not installed on this machine.
    Unavailable { feature: &'static str, reason: String },
    /// Decoding would exceed the memory-safety limits.
    TooLarge { width: u32, height: u32 },
    /// The caller passed a bad parameter (empty rect, zero size, ...).
    InvalidArgument(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(m) => write!(f, "I/O error: {m}"),
            Error::Corrupt(m) => write!(f, "damaged or unreadable file: {m}"),
            Error::Unsupported(m) => write!(f, "not supported: {m}"),
            Error::Unavailable { feature, reason } => write!(f, "{feature} is unavailable: {reason}"),
            Error::TooLarge { width, height } => write!(f, "image too large to process safely ({width}x{height})"),
            Error::InvalidArgument(m) => write!(f, "invalid argument: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

impl From<image::ImageError> for Error {
    fn from(e: image::ImageError) -> Self {
        use image::error::LimitErrorKind;
        match e {
            image::ImageError::Limits(l) => match l.kind() {
                LimitErrorKind::DimensionError | LimitErrorKind::InsufficientMemory => {
                    Error::TooLarge { width: 0, height: 0 }
                }
                _ => Error::Corrupt(l.to_string()),
            },
            image::ImageError::Unsupported(u) => Error::Unsupported(u.to_string()),
            image::ImageError::IoError(io) if io.kind() == std::io::ErrorKind::UnexpectedEof => {
                Error::Corrupt("file ends unexpectedly".into())
            }
            image::ImageError::IoError(io) => Error::Io(io.to_string()),
            other => Error::Corrupt(other.to_string()),
        }
    }
}
