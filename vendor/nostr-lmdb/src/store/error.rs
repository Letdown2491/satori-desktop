// Copyright (c) 2024 Michael Dilger
// Copyright (c) 2022-2023 Yuki Kishimoto
// Copyright (c) 2023-2025 Rust Nostr Developers
// Distributed under the MIT software license

use std::num::TryFromIntError;
use std::{fmt, io};

use async_utility::tokio::task::JoinError;
use flatbuffers::InvalidFlatbuffer;
use tokio::sync::oneshot;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MigrationError {
    /// Database version is newer than supported one
    NewerVersion {
        /// Current version of the database
        current_version: u64,
        /// Newer version of the database
        new_version: u64,
    },
}

impl std::error::Error for MigrationError {}

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NewerVersion {
                current_version,
                new_version,
            } => write!(
                f,
                "Database version {current_version} is newer than supported version {new_version}."
            ),
        }
    }
}

/// Missing field
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum MissingField {
    /// ID
    Id,
    /// Public key
    Pubkey,
    /// Tags
    Tags,
    /// Content
    Content,
    /// Signature
    Sig,
}

impl fmt::Display for MissingField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Id => write!(f, "id"),
            Self::Pubkey => write!(f, "pubkey"),
            Self::Tags => write!(f, "tags"),
            Self::Content => write!(f, "content"),
            Self::Sig => write!(f, "sig"),
        }
    }
}

#[derive(Debug)]
pub(crate) enum StoreError {
    Protocol(nostr::error::Error),
    Io(io::Error),
    Heed(heed::Error),
    Thread(JoinError),
    FlatBuffer(InvalidFlatbuffer),
    TryFromInt(TryFromIntError),
    OneshotRecv(oneshot::error::RecvError),
    Migration(MigrationError),
    FlatBufFieldNotFound(MissingField),
    FlumeSend,
    WrongEventKind,
    NotFound,
    BatchTransactionFailed,
}

impl std::error::Error for StoreError {}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(e) => e.fmt(f),
            Self::Io(e) => write!(f, "{e}"),
            Self::Heed(e) => write!(f, "{e}"),
            Self::FlatBuffer(e) => write!(f, "{e}"),
            Self::TryFromInt(e) => write!(f, "{e}"),
            Self::Thread(e) => write!(f, "{e}"),
            Self::OneshotRecv(e) => write!(f, "{e}"),
            Self::Migration(e) => write!(f, "Migration error: {e}"),
            Self::FlatBufFieldNotFound(field) => write!(f, "flatbuffer '{field}' field not found"),
            Self::FlumeSend => write!(f, "flume channel send error"),
            Self::NotFound => write!(f, "Not found"),
            Self::WrongEventKind => write!(f, "Wrong event kind"),
            Self::BatchTransactionFailed => write!(f, "Batched transaction failed"),
        }
    }
}

impl From<nostr::error::Error> for StoreError {
    fn from(e: nostr::error::Error) -> Self {
        Self::Protocol(e)
    }
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<heed::Error> for StoreError {
    fn from(e: heed::Error) -> Self {
        Self::Heed(e)
    }
}

impl From<JoinError> for StoreError {
    fn from(e: JoinError) -> Self {
        Self::Thread(e)
    }
}

impl From<InvalidFlatbuffer> for StoreError {
    fn from(e: InvalidFlatbuffer) -> Self {
        Self::FlatBuffer(e)
    }
}

impl From<TryFromIntError> for StoreError {
    fn from(e: TryFromIntError) -> Self {
        Self::TryFromInt(e)
    }
}

impl From<oneshot::error::RecvError> for StoreError {
    fn from(e: oneshot::error::RecvError) -> Self {
        Self::OneshotRecv(e)
    }
}

impl From<StoreError> for nostr_database::error::Error {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Protocol(e) => e.into(),
            StoreError::Io(e) => e.into(),
            StoreError::Migration(e) => Self::migration(e),
            e => Self::storage(e),
        }
    }
}
