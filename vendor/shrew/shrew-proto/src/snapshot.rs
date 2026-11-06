//! Finds where a Ghostty snapshot ends inside a byte stream.
//!
//! Layout: `"GHOSTSNP"` + u16 version, then records of
//! `tag: u16, len: u32, crc32c: u32, payload[len]` until the FINISH record.
//! Integers are little-endian. Checksums are verified by libghostty-vt when
//! the snapshot is decoded; this framer only needs the boundaries.

const MAGIC: &[u8; 8] = b"GHOSTSNP";
const ENVELOPE: usize = 10;
const RECORD_HEADER: usize = 10;
const TAG_FINISH: u16 = 6;
/// Refuse absurd snapshots rather than buffering forever.
const MAX_SNAPSHOT: usize = 512 << 20;

#[derive(Debug, PartialEq, Eq)]
pub enum Framed {
    /// Need more bytes.
    Pending,
    /// The whole snapshot, plus whatever followed it in the stream.
    Complete {
        snapshot: Vec<u8>,
        rest: Vec<u8>,
    },
    /// The stream doesn't start with a snapshot; here are its bytes.
    NotSnapshot(Vec<u8>),
    Invalid(String),
}

#[derive(Debug, Default)]
pub struct SnapshotFramer {
    buf: Vec<u8>,
    /// Offset of the next record header to inspect.
    cursor: usize,
}

impl SnapshotFramer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes buffered so far (for progress UI on slow links).
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Framed {
        self.buf.extend_from_slice(bytes);
        if self.cursor == 0 {
            let n = self.buf.len().min(MAGIC.len());
            if self.buf[..n] != MAGIC[..n] {
                return Framed::NotSnapshot(std::mem::take(&mut self.buf));
            }
            if self.buf.len() < ENVELOPE {
                return Framed::Pending;
            }
            self.cursor = ENVELOPE;
        }
        loop {
            if self.buf.len() < self.cursor + RECORD_HEADER {
                return self.pending();
            }
            let h = &self.buf[self.cursor..self.cursor + RECORD_HEADER];
            let tag = u16::from_le_bytes([h[0], h[1]]);
            let len = u32::from_le_bytes([h[2], h[3], h[4], h[5]]) as usize;
            let end = self.cursor + RECORD_HEADER + len;
            if end > MAX_SNAPSHOT {
                return Framed::Invalid(format!("snapshot larger than {MAX_SNAPSHOT} bytes"));
            }
            if self.buf.len() < end {
                return self.pending();
            }
            self.cursor = end;
            if tag == TAG_FINISH {
                let rest = self.buf.split_off(end);
                return Framed::Complete { snapshot: std::mem::take(&mut self.buf), rest };
            }
        }
    }

    fn pending(&self) -> Framed {
        if self.buf.len() > MAX_SNAPSHOT { Framed::Invalid("snapshot too large".into()) } else { Framed::Pending }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(tag: u16, payload: &[u8]) -> Vec<u8> {
        let mut v = tag.to_le_bytes().to_vec();
        v.extend((payload.len() as u32).to_le_bytes());
        v.extend([0u8; 4]);
        v.extend(payload);
        v
    }

    #[test]
    fn frames_across_chunks() {
        let mut snap = b"GHOSTSNP\x01\x00".to_vec();
        snap.extend(record(1, b"term"));
        snap.extend(record(5, b""));
        snap.extend(record(6, b""));
        let mut stream = snap.clone();
        stream.extend(b"after");
        for cut in 1..stream.len() {
            let mut f = SnapshotFramer::new();
            // Once complete, later bytes belong to the caller, not the framer.
            let done = match f.push(&stream[..cut]) {
                Framed::Complete { snapshot, mut rest } => {
                    rest.extend_from_slice(&stream[cut..]);
                    Framed::Complete { snapshot, rest }
                }
                _ => f.push(&stream[cut..]),
            };
            match done {
                Framed::Complete { snapshot, rest } => {
                    assert_eq!(snapshot, snap, "cut {cut}");
                    assert_eq!(rest, b"after".to_vec(), "cut {cut}");
                }
                other => panic!("cut {cut}: {other:?}"),
            }
        }
    }

    #[test]
    fn plain_vt_passes_through() {
        let mut f = SnapshotFramer::new();
        assert_eq!(f.push(b"\x1b[0mhello"), Framed::NotSnapshot(b"\x1b[0mhello".to_vec()));
    }
}
