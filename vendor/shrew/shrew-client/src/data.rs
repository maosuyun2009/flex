//! A terminal's data stream, kept alive across network drops with RESUME.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use shrew_proto::TERMINAL_SUBPROTOCOL;
use shrew_proto::apc::{Chunk, StreamMessage, StreamSplitter};
use shrew_proto::data::{self, DataWelcome, HandshakeError};
use shrew_proto::snapshot::{Framed, SnapshotFramer};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::endpoint::Ws;
use crate::{ClientError, Endpoint};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const DEAD_AFTER: Duration = Duration::from_secs(40);

#[derive(Debug, Clone, PartialEq)]
pub enum DataEvent {
    /// A brand-new stream started (first connect, or after a fresh ticket).
    /// The emulator must be reset: a snapshot follows.
    Started,
    /// A complete Ghostty snapshot of the terminal; restore it, then apply output.
    Snapshot(Vec<u8>),
    /// Program output, in order. In-band Rex messages are removed.
    Output(Vec<u8>),
    Resize {
        cols: u32,
        rows: u32,
    },
    Theme {
        foreground: Option<String>,
        background: Option<String>,
        cursor: Option<String>,
        palette: Vec<String>,
    },
    ChildExited {
        exit_code: i32,
    },
    /// The network dropped; we are trying to resume.
    Reconnecting,
    /// Resumed without losing a byte.
    Resumed,
    /// Resume is impossible. Call `DataStream::restart` with a new ticket.
    NeedsTicket,
    /// The block closed; the stream is over for good.
    Ended {
        reason: String,
    },
}

/// Handle to a running stream. Dropping it closes the stream with status 1000.
pub struct DataStream {
    ticket_tx: mpsc::UnboundedSender<String>,
    stop_tx: watch::Sender<bool>,
}

impl Drop for DataStream {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(true);
    }
}

impl DataStream {
    pub fn open(endpoint: Endpoint, ticket: String) -> (Self, mpsc::UnboundedReceiver<DataEvent>) {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (ticket_tx, ticket_rx) = mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = watch::channel(false);
        tokio::spawn(run(endpoint, ticket, ticket_rx, stop_rx, ev_tx));
        (Self { ticket_tx, stop_tx }, ev_rx)
    }

    /// Start over with a fresh ticket from `data.connect`.
    pub fn restart(&self, ticket: String) {
        let _ = self.ticket_tx.send(ticket);
    }
}

enum Outcome {
    Stopped,
    Ended(String),
    /// Connection dropped; resume may work.
    Dropped,
    /// Server says resuming won't work.
    Fresh,
}

async fn run(
    endpoint: Endpoint,
    first_ticket: String,
    mut ticket_rx: mpsc::UnboundedReceiver<String>,
    mut stop: watch::Receiver<bool>,
    tx: mpsc::UnboundedSender<DataEvent>,
) {
    let mut ticket = Some(first_ticket);
    let mut session: Option<(DataWelcome, u64)> = None; // welcome + bytes received
    let mut splitter = StreamSplitter::new(1);
    let mut framer: Option<SnapshotFramer> = None;
    let mut backoff = Duration::from_millis(250);

    loop {
        if *stop.borrow() {
            return;
        }
        let attempt = match (ticket.take(), &session) {
            (Some(t), _) => connect(&endpoint, Hand::Hello(t)).await.map(|r| (r, true)),
            (None, Some((w, n))) => connect(&endpoint, Hand::Resume(w.reconnect_token, *n)).await.map(|r| (r, false)),
            (None, None) => {
                let _ = tx.send(DataEvent::NeedsTicket);
                tokio::select! {
                    t = ticket_rx.recv() => match t { Some(t) => { ticket = Some(t); continue; } None => return },
                    _ = stop.changed() => return,
                }
            }
        };

        let (mut ws, welcome) = match attempt {
            Ok(((ws, w), fresh)) => {
                if fresh {
                    splitter = StreamSplitter::new(w.nonce);
                    framer = Some(SnapshotFramer::new());
                    session = Some((w, 0));
                    let _ = tx.send(DataEvent::Started);
                } else {
                    let n = session.as_ref().map(|s| s.1).unwrap_or(0);
                    session = Some((w, n));
                    let _ = tx.send(DataEvent::Resumed);
                }
                backoff = Duration::from_millis(250);
                (ws, w)
            }
            Err(ClientError::Handshake(HandshakeError::Rejected(r))) => {
                tracing::info!("data handshake rejected: {r}");
                session = None;
                continue; // asks for a ticket
            }
            Err(e) => {
                tracing::debug!("data connect failed: {e}");
                if session.is_none() && ticket.is_none() {
                    continue;
                }
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    t = ticket_rx.recv() => if let Some(t) = t { ticket = Some(t); session = None; },
                    _ = stop.changed() => return,
                }
                backoff = (backoff * 2).min(Duration::from_secs(5));
                continue;
            }
        };
        let _ = welcome;

        let outcome = pump(
            &mut ws,
            &mut splitter,
            &mut framer,
            session.as_mut().map(|s| &mut s.1).unwrap(),
            &tx,
            &mut stop,
            &mut ticket_rx,
            &mut ticket,
        )
        .await;
        match outcome {
            Outcome::Stopped => {
                let _ = ws.close(Some(close_frame(CloseCode::Normal))).await;
                return;
            }
            Outcome::Ended(reason) => {
                let _ = tx.send(DataEvent::Ended { reason });
                return;
            }
            Outcome::Dropped => {
                let _ = tx.send(DataEvent::Reconnecting);
            }
            Outcome::Fresh => session = None,
        }
    }
}

fn close_frame(code: CloseCode) -> tokio_tungstenite::tungstenite::protocol::CloseFrame {
    tokio_tungstenite::tungstenite::protocol::CloseFrame { code, reason: "".into() }
}

#[allow(clippy::too_many_arguments)]
async fn pump(
    ws: &mut Ws,
    splitter: &mut StreamSplitter,
    framer: &mut Option<SnapshotFramer>,
    received: &mut u64,
    tx: &mpsc::UnboundedSender<DataEvent>,
    stop: &mut watch::Receiver<bool>,
    ticket_rx: &mut mpsc::UnboundedReceiver<String>,
    ticket: &mut Option<String>,
) -> Outcome {
    let mut ticker = tokio::time::interval(Duration::from_secs(15));
    let mut last = tokio::time::Instant::now();
    loop {
        tokio::select! {
            m = ws.next() => {
                last = tokio::time::Instant::now();
                match m {
                    Some(Ok(Message::Binary(b))) => {
                        *received += b.len() as u64;
                        let live: Vec<u8> = match framer.as_mut().map(|f| f.push(&b)) {
                            None => b.to_vec(),
                            Some(Framed::Pending) => continue,
                            Some(Framed::Complete { snapshot, rest }) => {
                                *framer = None;
                                if tx.send(DataEvent::Snapshot(snapshot)).is_err() { return Outcome::Stopped; }
                                rest
                            }
                            Some(Framed::NotSnapshot(raw)) => { *framer = None; raw }
                            Some(Framed::Invalid(msg)) => {
                                tracing::warn!("bad snapshot: {msg}");
                                *framer = None;
                                return Outcome::Fresh;
                            }
                        };
                        for chunk in splitter.feed(&live) {
                            let ev = match chunk {
                                Chunk::Output(o) => DataEvent::Output(o),
                                Chunk::Message(StreamMessage::Resize { cols, rows, .. }) => DataEvent::Resize { cols, rows },
                                Chunk::Message(StreamMessage::Theme { foreground, background, cursor, palette, .. }) =>
                                    DataEvent::Theme { foreground, background, cursor, palette: palette.unwrap_or_default() },
                                Chunk::Message(StreamMessage::ChildExited { exit_code }) => DataEvent::ChildExited { exit_code },
                                Chunk::Message(StreamMessage::Unknown) => continue,
                            };
                            if tx.send(ev).is_err() { return Outcome::Stopped; }
                        }
                    }
                    Some(Ok(Message::Close(f))) => {
                        let (code, reason) = f.map(|f| (u16::from(f.code), f.reason.to_string())).unwrap_or((1005, String::new()));
                        return match code {
                            1000 | 4000 => Outcome::Ended(if reason.is_empty() { "stream ended".into() } else { reason }),
                            1008 if reason.contains("unavailable") => Outcome::Ended(reason),
                            1013 | 1002 | 1008 | 1011 => Outcome::Fresh,
                            _ => Outcome::Dropped,
                        };
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => return Outcome::Dropped,
                }
            }
            t = ticket_rx.recv() => {
                if let Some(t) = t { *ticket = Some(t); let _ = ws.close(Some(close_frame(CloseCode::Normal))).await; return Outcome::Fresh; }
            }
            _ = stop.changed() => return Outcome::Stopped,
            _ = ticker.tick() => {
                if last.elapsed() > DEAD_AFTER { return Outcome::Dropped; }
                if ws.send(Message::Ping(Default::default())).await.is_err() { return Outcome::Dropped; }
            }
        }
    }
}

enum Hand {
    Hello(String),
    Resume([u8; 16], u64),
}

async fn connect(endpoint: &Endpoint, hand: Hand) -> Result<(Ws, DataWelcome), ClientError> {
    let fut = async {
        let mut ws = endpoint.websocket("/api/data", TERMINAL_SUBPROTOCOL).await?;
        let frame = match &hand {
            Hand::Hello(t) => data::hello(&data::decode_ticket(t)?),
            Hand::Resume(tok, n) => data::resume(tok, *n),
        };
        ws.send(Message::binary(frame)).await?;
        loop {
            match ws.next().await {
                Some(Ok(Message::Binary(b))) => return Ok((ws, data::parse_reply(&b)?)),
                Some(Ok(Message::Close(_))) | None => return Err(ClientError::Closed),
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(e.into()),
            }
        }
    };
    tokio::time::timeout(HANDSHAKE_TIMEOUT * 2, fut).await.map_err(|_| ClientError::Timeout)?
}
