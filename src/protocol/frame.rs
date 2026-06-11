use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::macros::text_enum;

text_enum!(ErrorCode {
    UnknownCommand => "unknown-command",
    InvalidArgs => "invalid-args",
    PermissionDenied => "permission-denied",
    Forbidden => "forbidden",
    ApprovalPending => "approval-pending",
    NotFound => "not-found",
    HandlerError => "handler-error",
    TransportError => "transport-error",
});

/// One admin request: a command name, an optional correlation id, and JSON args.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestFrame {
    #[serde(default)]
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub args: Map<String, Value>,
}

impl RequestFrame {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        command: impl Into<String>,
        args: Map<String, Value>,
    ) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            args,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameError {
    pub code: ErrorCode,
    pub message: String,
}

/// One admin response, correlated to a request by `id`. `ok` discriminates;
/// exactly one of `data` / `error` is present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseFrame {
    pub id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<FrameError>,
}

impl ResponseFrame {
    #[must_use]
    pub fn ok(id: impl Into<String>, data: Value) -> Self {
        Self {
            id: id.into(),
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    #[must_use]
    pub fn error(id: impl Into<String>, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ok: false,
            data: None,
            error: Some(FrameError {
                code,
                message: message.into(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_defaults_id_and_args() {
        let parsed: RequestFrame =
            serde_json::from_str(r#"{"command":"groups-list"}"#).expect("parse");
        assert_eq!(parsed.command, "groups-list");
        assert_eq!(parsed.id, "");
        assert!(parsed.args.is_empty());
    }

    #[test]
    fn ok_response_round_trips() {
        let frame = ResponseFrame::ok("r1", serde_json::json!({"count": 2}));
        let encoded = serde_json::to_string(&frame).expect("encode");
        assert_eq!(encoded, r#"{"id":"r1","ok":true,"data":{"count":2}}"#);
        let back: ResponseFrame = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(back, frame);
    }

    #[test]
    fn error_response_carries_a_kebab_code() {
        let frame = ResponseFrame::error("r1", ErrorCode::UnknownCommand, "no such command");
        let encoded = serde_json::to_string(&frame).expect("encode");
        assert!(encoded.contains(r#""code":"unknown-command""#));
        assert!(encoded.contains(r#""ok":false"#));
        assert!(!encoded.contains("data"));
    }
}
