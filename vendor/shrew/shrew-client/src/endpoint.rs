use std::path::PathBuf;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use url::Url;

use crate::ClientError;

pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub(crate) type Ws = WebSocketStream<MaybeTlsStream<Box<dyn Io>>>;

/// Where a Rex server listens.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Endpoint {
    /// A local server's Unix socket.
    Unix(PathBuf),
    /// `http://host:port`, `https://name.tailnet.ts.net`, or the ws/wss forms.
    Url(Url),
}

impl Endpoint {
    /// Parse what a person types: a URL, `host:port`, a bare host, or a socket path.
    pub fn parse(input: &str) -> Result<Self, ClientError> {
        let s = input.trim();
        if s.is_empty() {
            return Err(ClientError::BadEndpoint("empty".into()));
        }
        if let Some(path) = s.strip_prefix("unix://") {
            let decoded = percent_decode(path);
            return Ok(Self::Unix(PathBuf::from(decoded)));
        }
        if s.starts_with('/') || s.starts_with('~') {
            return Ok(Self::Unix(expand_home(s)));
        }
        let with_scheme = if s.contains("://") {
            s.to_owned()
        } else if s.ends_with(".ts.net") {
            format!("https://{s}")
        } else if s.contains(':') {
            format!("http://{s}")
        } else {
            format!("http://{s}:7878")
        };
        let url = Url::parse(&with_scheme).map_err(|e| ClientError::BadEndpoint(format!("{s}: {e}")))?;
        match url.scheme() {
            "http" | "https" | "ws" | "wss" if url.host_str().is_some() => Ok(Self::Url(url)),
            other => Err(ClientError::BadEndpoint(format!("unsupported scheme {other}"))),
        }
    }

    /// The local server's default socket on this machine, if there is one.
    pub fn local_default() -> Option<Self> {
        if let Ok(v) = std::env::var("REX_SERVER")
            && let Ok(e) = Self::parse(&v) {
                return Some(e);
            }
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        let candidates = [
            home.join("Library/Application Support/rex/server.sock"),
            std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("rex/server.sock")).unwrap_or_default(),
            home.join(".local/state/rex/server.sock"),
        ];
        candidates.into_iter().find(|p| p.exists()).map(Self::Unix)
    }

    pub fn display(&self) -> String {
        match self {
            Self::Unix(p) => p.display().to_string(),
            Self::Url(u) => u.as_str().trim_end_matches('/').to_owned(),
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::Unix(_))
    }

    pub(crate) async fn websocket(&self, path: &str, subprotocol: &str) -> Result<Ws, ClientError> {
        let (url, io): (String, Box<dyn Io>) = match self {
            Self::Unix(p) => {
                #[cfg(unix)]
                {
                    let s = tokio::net::UnixStream::connect(p).await?;
                    (format!("ws://localhost{path}"), Box::new(s))
                }
                #[cfg(not(unix))]
                {
                    let _ = p;
                    return Err(ClientError::BadEndpoint("unix sockets are unsupported here".into()));
                }
            }
            Self::Url(u) => {
                let tls = matches!(u.scheme(), "https" | "wss");
                let host = u.host_str().ok_or_else(|| ClientError::BadEndpoint("no host".into()))?;
                let port = u.port().unwrap_or(if tls { 443 } else { 80 });
                let tcp = tokio::net::TcpStream::connect((host, port)).await?;
                tcp.set_nodelay(true)?;
                let scheme = if tls { "wss" } else { "ws" };
                let host_part = if host.contains(':') { format!("[{host}]") } else { host.to_owned() };
                (format!("{scheme}://{host_part}:{port}{path}"), Box::new(tcp))
            }
        };
        install_crypto();
        let mut req = url.into_client_request()?;
        req.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_str(subprotocol).unwrap());
        let mut cfg = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
        cfg.max_message_size = Some(16 << 20);
        cfg.max_frame_size = Some(16 << 20);
        let (ws, resp) = tokio_tungstenite::client_async_tls_with_config(req, io, Some(cfg), None).await?;
        let agreed = resp.headers().get("Sec-WebSocket-Protocol").and_then(|v| v.to_str().ok());
        if agreed != Some(subprotocol) {
            return Err(ClientError::Protocol(format!("server did not accept subprotocol {subprotocol}")));
        }
        Ok(ws)
    }
}

fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn expand_home(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(h) = std::env::var_os("HOME") {
            return PathBuf::from(h).join(rest);
        }
    PathBuf::from(s)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_human_input() {
        let u = |s| match Endpoint::parse(s).unwrap() {
            Endpoint::Url(u) => u.to_string(),
            Endpoint::Unix(p) => p.display().to_string(),
        };
        assert_eq!(u("100.64.0.2:7878"), "http://100.64.0.2:7878/");
        assert_eq!(u("nixos-server"), "http://nixos-server:7878/");
        assert_eq!(u("box.tail1234.ts.net"), "https://box.tail1234.ts.net/");
        assert_eq!(
            u("unix:///Users/a/Library/Application%20Support/rex/server.sock"),
            "/Users/a/Library/Application Support/rex/server.sock"
        );
        assert!(Endpoint::parse("ftp://x").is_err());
    }
}
