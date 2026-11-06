//! Control connection envelopes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ClientInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Messages a client sends.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        #[serde(skip_serializing_if = "Option::is_none")]
        reconnect_token: Option<String>,
        client: ClientInfo,
        api_revision: u32,
        api_min_revision: u32,
    },
    Request {
        id: String,
        method: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
    },
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Limits {
    pub max_message_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct WelcomeClient {
    pub id: String,
    pub reconnect_token: String,
    #[serde(default)]
    pub resumed: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Welcome {
    pub protocol: String,
    pub limits: Limits,
    #[serde(default)]
    pub client: Option<WelcomeClient>,
    #[serde(default)]
    pub features: Option<Vec<String>>,
    #[serde(default)]
    pub api_revision: u32,
    #[serde(default)]
    pub api_server_revision: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ApiError {
    pub code: String,
    pub message: String,
    pub status: u16,
}

impl ApiError {
    pub fn local(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), status: 0 }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Response {
    pub id: String,
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub payload: Option<Value>,
    #[serde(default)]
    pub error: Option<ApiError>,
}

/// An event's name and fields: `{"type": ..., "payload": ...}`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct EventBody {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct EventEnvelope {
    pub seq: u64,
    pub payload: EventBody,
}

/// Messages the server sends. Unknown types are kept so callers can skip them.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    Welcome(Welcome),
    Response(Response),
    Event(EventEnvelope),
    Unknown(String),
}

impl ServerMessage {
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        let v: Value = serde_json::from_str(text)?;
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("").to_owned();
        Ok(match kind.as_str() {
            "welcome" => ServerMessage::Welcome(serde_json::from_value(v)?),
            "response" => ServerMessage::Response(serde_json::from_value(v)?),
            "event" => ServerMessage::Event(serde_json::from_value(v)?),
            _ => ServerMessage::Unknown(kind),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_shape() {
        let m = ClientMessage::Hello {
            reconnect_token: None,
            client: ClientInfo { kind: Some("app".into()), name: Some("Shrew".into()), ..Default::default() },
            api_revision: 8,
            api_min_revision: 8,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"{"type":"hello","client":{"kind":"app","name":"Shrew"},"api_revision":8,"api_min_revision":8}"#);
    }

    #[test]
    fn parses_server_messages() {
        let w = ServerMessage::parse(r#"{"type":"welcome","protocol":"rex.control.v1","limits":{"max_message_bytes":1048576},"client":{"id":"client:x","reconnect_token":"t"},"api_revision":8}"#).unwrap();
        assert!(matches!(w, ServerMessage::Welcome(Welcome { api_revision: 8, .. })));
        let r = ServerMessage::parse(
            r#"{"type":"response","id":"2","seq":0,"error":{"code":"not_found","message":"block not found","status":404}}"#,
        )
        .unwrap();
        match r {
            ServerMessage::Response(r) => assert_eq!(r.error.unwrap().status, 404),
            _ => panic!(),
        }
        let e = ServerMessage::parse(
            r#"{"type":"event","seq":1,"payload":{"type":"session_created","payload":{"session":{"session_id":"s","label":"work"}}}}"#,
        )
        .unwrap();
        assert!(matches!(e, ServerMessage::Event(EventEnvelope { seq: 1, .. })));
        assert_eq!(ServerMessage::parse(r#"{"type":"future"}"#).unwrap(), ServerMessage::Unknown("future".into()));
    }
}
