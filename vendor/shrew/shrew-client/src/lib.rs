//! Async client for Rex servers: one control connection plus resumable data streams.

mod connection;
mod data;
mod endpoint;
mod error;

pub use connection::{
    AttachedSession, Bootstrap, ConnEvent, ConnectOptions, Connection, DataTicket, KeyPress, Mods, char_code, platform_name,
};
pub use data::{DataEvent, DataStream};
pub use endpoint::Endpoint;
pub use error::ClientError;
pub use shrew_proto as proto;
