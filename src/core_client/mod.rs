//! Client for herdr-core (`hc`), the terminal runtime Herdr can show as panes.
//!
//! Herdr connects to an already running core over its public socket API
//! (JSON-RPC, then framed mode after `hello`). It never starts a core and never
//! runs the `hc` CLI, so listing or showing core terminals cannot start one.

#![allow(dead_code)] // Wired up step by step; removed once every caller exists.

pub(crate) mod connection;
#[cfg(all(test, unix))]
pub(crate) mod fake_core;
pub(crate) mod model;
pub(crate) mod protocol;
pub(crate) mod resync;
pub(crate) mod rpc;
pub(crate) mod wire;
