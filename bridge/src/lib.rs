//! Shared library surface for the agent-buddy bridge.
//!
//! Two binaries build on this crate:
//!   * `agent-buddy`     — the daemon + CLI (`src/main.rs`)
//!   * `agent-buddy-app` — the desktop control panel (`src/bin/app.rs`)
//!
//! Both speak the same IPC protocol ([`ipc`]) to the one long-running daemon
//! that owns the BLE radio, so the GUI never touches Bluetooth itself — it is a
//! thin client over the socket the daemon already publishes.

pub mod agent;
pub mod ble;
pub mod client;
pub mod daemon;
pub mod hook;
pub mod ingest;
pub mod ipc;
pub mod ota;
pub mod packs;
pub mod power;
pub mod protocol;
pub mod selfupdate;
pub mod setup;
pub mod state;
pub mod update;
#[cfg(feature = "gui")]
pub mod widget;
