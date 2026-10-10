//! The orgtree mail hub, v2: org-to-org mail across machines.
//!
//! Protocol-compatible with the v1 hub (Python, FastAPI + SQLite): the same
//! routes, credentials, refusals, custody and receipt rules, so every
//! existing client keeps working unchanged. Records live in PostgreSQL;
//! attachment bytes stay files on disk.
//!
//! The design rules carried over from v1 (docs/mailserver-spec.md):
//! - Instances DIAL OUT and long-poll; nothing ever connects back to one.
//! - Joining is open; ADDRESSES are owned: the hub stores sha256(secret)
//!   and compares the full digest, never the 6-character display suffix.
//! - One multiplexed long poll per instance carries the mail for all its
//!   orgs, the receipts its senders are owed, and the roster with presence.
//! - Delivery is at-least-once: a message stays queued until the recipient
//!   acks custody; duplicates are the client's to collapse.
//! - `received_at` (hub clock) orders; `sent_at` is the sender's claim.
//! - ⚠ The hub sees every message in plaintext, and the read-only operator
//!   UI at `/` is unauthenticated by ruling: hub access is read access to
//!   all mail on the closed network it is built for.

pub mod api;
pub mod auth;
pub mod blobs;
pub mod clock;
pub mod config;
pub mod db;
pub mod import;
pub mod log;
pub mod presence;
pub mod push;
pub mod server;
pub mod sweep;
pub mod wire;

pub use api::Hub;
pub use config::Config;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
