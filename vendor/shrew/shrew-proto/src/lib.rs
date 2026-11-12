//! Wire types and codecs for the Rex server protocol (API revisions 8–13).
//!
//! This crate is pure: no I/O, no async. It describes the control connection
//! (`rex.control.v1`, JSON over WebSocket text frames), the data connection
//! handshake (`rex.data.terminal.v1`, fixed binary frames), and the in-band
//! `ESC _ rex1;<nonce>;<json> ESC \` messages carried on a terminal stream.

pub mod apc;
pub mod control;
pub mod data;
pub mod model;
pub mod snapshot;

pub use control::*;
pub use model::*;

/// Revision 13 schemas are checked by flex's opt-in Rex integration tests.
pub const API_REVISION: u32 = 13;
/// The oldest revision whose shapes we can still parse.
pub const API_MIN_REVISION: u32 = 8;
pub const CONTROL_SUBPROTOCOL: &str = "rex.control.v1";
pub const TERMINAL_SUBPROTOCOL: &str = "rex.data.terminal.v1";
pub const BYTES_SUBPROTOCOL: &str = "rex.data.bytes.v1";
pub const TERMINAL_CREATOR: &str = "com.superlogical.terminal";
pub const SHELL_FLAVOR: &str = "com.superlogical.terminal.shell";
