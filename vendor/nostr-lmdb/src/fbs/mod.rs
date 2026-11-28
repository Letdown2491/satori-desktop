// Copyright (c) 2022-2023 Yuki Kishimoto
// Copyright (c) 2023-2025 Rust Nostr Developers
// Distributed under the MIT software license

#[allow(
    unused_imports,
    dead_code,
    clippy::all,
    unsafe_code,
    missing_docs,
    unsafe_op_in_unsafe_fn
)]
mod event_generated;

pub(crate) use self::event_generated::event_fbs;
