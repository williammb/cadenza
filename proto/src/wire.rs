//! NDJSON envelopes — Request, Response, Event, ErrorBody.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::WIRE_VERSION;

/// Client → server frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    /// Correlation id. May be `null` for the initial `hello`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub op: String,
    /// Op-specific args — deserialized to the typed struct in `ops`
    /// during dispatch.
    #[serde(default)]
    pub args: Value,
}

/// Server → client reply to a `Request`. `ok` distinguishes success
/// from error; exactly one of `result` / `error` is present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub v: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

/// Server → client unsolicited push (id is `null`). Used during
/// `await_decision` to surface `proposta_pendente` / `proposta_decidida`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub v: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>, // always None — present for shape uniformity
    pub event: String,
    #[serde(default)]
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

impl Request {
    pub fn new<A: Serialize>(id: Option<String>, op: &str, args: A) -> serde_json::Result<Self> {
        Ok(Self {
            v: WIRE_VERSION,
            id,
            op: op.to_string(),
            args: serde_json::to_value(args)?,
        })
    }
}

impl Response {
    pub fn ok<T: Serialize>(id: Option<String>, result: T) -> serde_json::Result<Self> {
        Ok(Self {
            v: WIRE_VERSION,
            id,
            ok: true,
            result: Some(serde_json::to_value(result)?),
            error: None,
        })
    }

    pub fn err(id: Option<String>, error: ErrorBody) -> Self {
        Self {
            v: WIRE_VERSION,
            id,
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

impl Event {
    pub fn new<D: Serialize>(name: &str, data: D) -> serde_json::Result<Self> {
        Ok(Self {
            v: WIRE_VERSION,
            id: None,
            event: name.to_string(),
            data: serde_json::to_value(data)?,
        })
    }
}

impl ErrorBody {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
        }
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }
}

/// Untagged frame for the client side: a single line off the wire is
/// either a Response (correlated by id) or an Event (id is null).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ServerFrame {
    Response(Response),
    Event(Event),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let req = Request::new(
            Some("1".into()),
            "list_tasks",
            serde_json::json!({"estado": "fazendo"}),
        )
        .unwrap();
        let s = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back.op, "list_tasks");
        assert_eq!(back.id.as_deref(), Some("1"));
    }

    #[test]
    fn response_ok_skips_error_field() {
        let r = Response::ok(Some("1".into()), serde_json::json!({"x": 1})).unwrap();
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"ok\":true"));
        assert!(!s.contains("\"error\""));
    }

    #[test]
    fn server_frame_distinguishes_event_from_response() {
        let event_line = r#"{"v":1,"id":null,"event":"proposta_pendente","data":{"proposta_id":"P-1"}}"#;
        let resp_line = r#"{"v":1,"id":"3","ok":true,"result":{"decisao":"aceita"}}"#;
        match serde_json::from_str::<ServerFrame>(event_line).unwrap() {
            ServerFrame::Event(e) => assert_eq!(e.event, "proposta_pendente"),
            _ => panic!("expected event"),
        }
        match serde_json::from_str::<ServerFrame>(resp_line).unwrap() {
            ServerFrame::Response(r) => assert!(r.ok),
            _ => panic!("expected response"),
        }
    }
}
