//! Async client for Rex servers: one control connection plus resumable data streams.

mod bytes;
mod connection;
mod data;
mod endpoint;
mod error;

pub use bytes::{download_file, upload_file, open_upload_source};
pub use connection::{
    AttachedSession, Bootstrap, ConnEvent, ConnectOptions, Connection, DataTicket, KeyPress, Mods, Reply, char_code, platform_name,
};
pub use data::{DataEvent, DataStream};
pub use endpoint::Endpoint;
pub use error::ClientError;
pub use shrew_proto as proto;
