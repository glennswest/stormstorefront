//! The storefront on stormcos: every role of every container, in one crate.
//! `src/main.rs` picks the role from its first argument.

pub mod api;
pub mod cache;
pub mod content;
pub mod edge;
pub mod exporter;
pub mod http;
pub mod resp;
pub mod sidecars;
pub mod store;
pub mod util;
pub mod validate;
pub mod web;

/// Every role the binary answers, as `command: [/bin/stormstorefront, <role>]`.
pub const ROLES: &[&str] = &[
    "edge",
    "config-reloader",
    "validate-config",
    "web",
    "content-init",
    "content-sync",
    "exporter",
    "api",
    "access-log",
    "cache-warmer",
    "wait-for-deps",
    "store",
    "backup",
    "cache",
    "sleep",
    "validate",
    "version",
];
