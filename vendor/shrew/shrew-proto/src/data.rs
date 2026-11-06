//! Data connection handshake frames. Integers are little-endian.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub const HELLO: u8 = 0x01;
pub const RESUME: u8 = 0x02;
pub const WELCOME: u8 = 0x81;
pub const REJECT: u8 = 0x82;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HandshakeError {
    #[error("ticket is not 16 bytes of base64url")]
    BadTicket,
    #[error("malformed handshake reply")]
    Malformed,
    #[error("server rejected the data connection: {0}")]
    Rejected(RejectReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    UnknownTicket,
    AlreadyConnected,
    StreamUnavailable,
    Limit,
    Malformed,
    ServerFailed,
    WrongSubprotocol,
    Other(u8),
}

impl RejectReason {
    pub fn from_byte(b: u8) -> Self {
        match b {
            0x01 => Self::UnknownTicket,
            0x02 => Self::AlreadyConnected,
            0x03 => Self::StreamUnavailable,
            0x04 => Self::Limit,
            0x05 => Self::Malformed,
            0x06 => Self::ServerFailed,
            0x07 => Self::WrongSubprotocol,
            b => Self::Other(b),
        }
    }
    /// Whether a fresh ticket could plausibly succeed.
    pub fn retryable(self) -> bool {
        !matches!(self, Self::StreamUnavailable | Self::WrongSubprotocol)
    }
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::UnknownTicket => "ticket unknown, expired, or already used",
            Self::AlreadyConnected => "already connected to this stream",
            Self::StreamUnavailable => "stream unavailable",
            Self::Limit => "connection limit reached",
            Self::Malformed => "malformed handshake",
            Self::ServerFailed => "server failure",
            Self::WrongSubprotocol => "ticket issued for another subprotocol",
            Self::Other(b) => return write!(f, "unknown reason 0x{b:02x}"),
        };
        f.write_str(s)
    }
}

pub fn decode_ticket(ticket: &str) -> Result<[u8; 16], HandshakeError> {
    let bytes = URL_SAFE_NO_PAD.decode(ticket.trim_end_matches('=')).map_err(|_| HandshakeError::BadTicket)?;
    bytes.try_into().map_err(|_| HandshakeError::BadTicket)
}

pub fn hello(ticket: &[u8; 16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(17);
    v.push(HELLO);
    v.extend_from_slice(ticket);
    v
}

pub fn resume(token: &[u8; 16], received: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(25);
    v.push(RESUME);
    v.extend_from_slice(token);
    v.extend_from_slice(&received.to_le_bytes());
    v
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataWelcome {
    pub reconnect_token: [u8; 16],
    pub resume_window_ms: u32,
    pub nonce: u64,
}

pub fn parse_reply(msg: &[u8]) -> Result<DataWelcome, HandshakeError> {
    match msg.first() {
        Some(&WELCOME) if msg.len() == 32 => {
            let mut token = [0u8; 16];
            token.copy_from_slice(&msg[1..17]);
            // 3 reserved zero bytes at 17..20
            let window = u32::from_le_bytes(msg[20..24].try_into().unwrap());
            let nonce = u64::from_le_bytes(msg[24..32].try_into().unwrap());
            if nonce == 0 {
                return Err(HandshakeError::Malformed);
            }
            Ok(DataWelcome { reconnect_token: token, resume_window_ms: window, nonce })
        }
        Some(&REJECT) if msg.len() >= 2 => Err(HandshakeError::Rejected(RejectReason::from_byte(msg[1]))),
        _ => Err(HandshakeError::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_roundtrip() {
        let t = decode_ticket("AAECAwQFBgcICQoLDA0ODw").unwrap();
        assert_eq!(t, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        assert_eq!(hello(&t).len(), 17);
        assert_eq!(resume(&t, 7)[17..], 7u64.to_le_bytes());
    }

    #[test]
    fn welcome_and_reject() {
        let mut w = vec![WELCOME];
        w.extend_from_slice(&[9; 16]);
        w.extend_from_slice(&[0, 0, 0]);
        w.extend_from_slice(&60_000u32.to_le_bytes());
        w.extend_from_slice(&42u64.to_le_bytes());
        let p = parse_reply(&w).unwrap();
        assert_eq!((p.resume_window_ms, p.nonce), (60_000, 42));
        assert_eq!(parse_reply(&[REJECT, 2]), Err(HandshakeError::Rejected(RejectReason::AlreadyConnected)));
        assert_eq!(parse_reply(&[0x81, 1]), Err(HandshakeError::Malformed));
    }
}
