//! Login/session errors.

use std::fmt;

#[derive(Debug)]
pub enum LoginError {
    InvalidBunkerUri(String),
    InvalidNsec,
    Connect(String),
    Store(String),
    Resume(String),
}

impl fmt::Display for LoginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBunkerUri(s) => write!(f, "not a valid bunker:// URI: {s}"),
            Self::InvalidNsec => write!(f, "not a valid nsec or hex secret key"),
            Self::Connect(s) => write!(f, "bunker connect failed: {s}"),
            Self::Store(s) => write!(f, "secret store: {s}"),
            Self::Resume(s) => write!(f, "session resume failed: {s}"),
        }
    }
}

impl std::error::Error for LoginError {}

impl From<keyring::Error> for LoginError {
    fn from(e: keyring::Error) -> Self {
        Self::Store(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, LoginError>;
