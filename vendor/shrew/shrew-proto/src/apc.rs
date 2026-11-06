//! Splits a terminal stream into program output and Rex's in-band messages.
//!
//! Rex messages are `ESC _ rex1;<nonce>;<json> ESC \`. Only those carrying the
//! channel nonce from the data WELCOME are genuine; anything else (including a
//! program printing a look-alike) is passed through untouched for the terminal
//! emulator to handle (or ignore) as a normal APC.

use serde::Deserialize;

const PREFIX: &[u8] = b"\x1b_rex1;";
/// Cap on a single buffered in-band message; anything larger is passed through.
const MAX_MESSAGE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamMessage {
    Resize {
        cols: u32,
        rows: u32,
        #[serde(default)]
        cell_width_px: u32,
        #[serde(default)]
        cell_height_px: u32,
        #[serde(default)]
        owner: Option<String>,
    },
    Theme {
        #[serde(default)]
        foreground: Option<String>,
        #[serde(default)]
        background: Option<String>,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default)]
        palette: Option<Vec<String>>,
        #[serde(default)]
        scheme: Option<String>,
    },
    ChildExited {
        exit_code: i32,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Chunk {
    Output(Vec<u8>),
    Message(StreamMessage),
}

#[derive(Debug)]
pub struct StreamSplitter {
    nonce: String,
    /// Bytes that might be the start of an in-band message, held until we know.
    pending: Vec<u8>,
}

impl StreamSplitter {
    pub fn new(nonce: u64) -> Self {
        Self { nonce: nonce.to_string(), pending: Vec::new() }
    }

    pub fn set_nonce(&mut self, nonce: u64) {
        self.nonce = nonce.to_string();
    }

    /// Feed bytes; returns output and messages in stream order.
    pub fn feed(&mut self, input: &[u8]) -> Vec<Chunk> {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(input);
        let mut out = Vec::new();
        let mut text_start = 0;
        let mut i = 0;
        while i < buf.len() {
            if buf[i] != 0x1b {
                i += 1;
                continue;
            }
            let rest = &buf[i..];
            let n = rest.len().min(PREFIX.len());
            if rest[..n] != PREFIX[..n] {
                i += 1;
                continue;
            }
            if rest.len() < PREFIX.len() {
                // Possible prefix cut off at the end of this chunk.
                push_output(&mut out, &buf[text_start..i]);
                self.pending = rest.to_vec();
                return out;
            }
            // Find the ST terminator (ESC \).
            match find_st(&rest[PREFIX.len()..]) {
                Some(end) => {
                    let body = &rest[PREFIX.len()..PREFIX.len() + end];
                    let total = PREFIX.len() + end + 2;
                    if let Some(msg) = self.parse_body(body) {
                        push_output(&mut out, &buf[text_start..i]);
                        out.push(Chunk::Message(msg));
                        i += total;
                        text_start = i;
                    } else {
                        i += total; // foreign nonce: leave as output
                    }
                }
                None if rest.len() > MAX_MESSAGE => i += 1,
                None => {
                    push_output(&mut out, &buf[text_start..i]);
                    self.pending = rest.to_vec();
                    return out;
                }
            }
        }
        push_output(&mut out, &buf[text_start..]);
        out
    }

    fn parse_body(&self, body: &[u8]) -> Option<StreamMessage> {
        let s = std::str::from_utf8(body).ok()?;
        let (nonce, json) = s.split_once(';')?;
        if nonce != self.nonce {
            return None;
        }
        Some(serde_json::from_str(json).unwrap_or(StreamMessage::Unknown))
    }
}

fn find_st(b: &[u8]) -> Option<usize> {
    b.windows(2).position(|w| w == b"\x1b\\")
}

fn push_output(out: &mut Vec<Chunk>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if let Some(Chunk::Output(prev)) = out.last_mut() {
        prev.extend_from_slice(bytes);
    } else {
        out.push(Chunk::Output(bytes.to_vec()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(nonce: u64, json: &str) -> Vec<u8> {
        format!("\x1b_rex1;{nonce};{json}\x1b\\").into_bytes()
    }

    #[test]
    fn splits_and_filters_nonce() {
        let mut s = StreamSplitter::new(77);
        let mut input = b"hello ".to_vec();
        input.extend(msg(77, r#"{"type":"resize","cols":80,"rows":24}"#));
        input.extend(b"world");
        input.extend(msg(5, r#"{"type":"child_exited","exit_code":1}"#));
        let out = s.feed(&input);
        assert_eq!(out[0], Chunk::Output(b"hello ".to_vec()));
        assert!(matches!(out[1], Chunk::Message(StreamMessage::Resize { cols: 80, rows: 24, .. })));
        match &out[2] {
            Chunk::Output(b) => assert!(b.starts_with(b"world\x1b_rex1;5;")),
            _ => panic!(),
        }
    }

    #[test]
    fn handles_every_split_point() {
        let mut full = b"ab".to_vec();
        full.extend(msg(9, r#"{"type":"child_exited","exit_code":3}"#));
        full.extend(b"cd\x1b[0m");
        for cut in 0..full.len() {
            let mut s = StreamSplitter::new(9);
            let mut out = s.feed(&full[..cut]);
            out.extend(s.feed(&full[cut..]));
            let text: Vec<u8> = out
                .iter()
                .flat_map(|c| match c {
                    Chunk::Output(b) => b.clone(),
                    _ => vec![],
                })
                .collect();
            assert_eq!(text, b"abcd\x1b[0m", "cut {cut}");
            assert_eq!(out.iter().filter(|c| matches!(c, Chunk::Message(StreamMessage::ChildExited { exit_code: 3 }))).count(), 1);
        }
    }

    #[test]
    fn unknown_type_is_skippable() {
        let mut s = StreamSplitter::new(1);
        let out = s.feed(&msg(1, r#"{"type":"sparkles","x":1}"#));
        assert_eq!(out, vec![Chunk::Message(StreamMessage::Unknown)]);
    }
}
