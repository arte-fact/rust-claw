use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ids::UserId;
use super::message::{MessageKind, Seq};

#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    #[error("malformed {kind} content: {source}")]
    Malformed {
        kind: String,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum InboundContent {
    Chat(ChatContent),
    Task(TaskContent),
    Webhook(WebhookContent),
    System(SystemResult),
    Raw(Value),
}

impl InboundContent {
    pub fn parse(kind: &str, json: &str) -> Result<Self, ContentError> {
        let malformed = |source| ContentError::Malformed {
            kind: kind.to_owned(),
            source,
        };
        match kind.parse::<MessageKind>() {
            Ok(MessageKind::Chat) => serde_json::from_str(json)
                .map(Self::Chat)
                .map_err(malformed),
            Ok(MessageKind::Task) => serde_json::from_str(json)
                .map(Self::Task)
                .map_err(malformed),
            Ok(MessageKind::Webhook) => serde_json::from_str(json)
                .map(Self::Webhook)
                .map_err(malformed),
            Ok(MessageKind::System) => serde_json::from_str(json)
                .map(Self::System)
                .map_err(malformed),
            Err(_) => serde_json::from_str(json).map(Self::Raw).map_err(malformed),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatContent {
    pub sender: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<UserId>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub is_from_me: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quoted: Option<QuotedMessage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotedMessage {
    pub sender: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskContent {
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(default = "default_true")]
    pub wake_agent: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebhookContent {
    pub source: String,
    pub event: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemResult {
    pub action: String,
    pub status: String,
    #[serde(default)]
    pub result: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboundContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<Operation>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl OutboundContent {
    #[must_use]
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            files: Vec::new(),
            operation: None,
            extra: serde_json::Map::new(),
        }
    }

    pub fn parse(json: &str) -> Result<Self, ContentError> {
        serde_json::from_str(json).map_err(|source| ContentError::Malformed {
            kind: "outbound".to_owned(),
            source,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Operation {
    AskQuestion {
        question_id: String,
        title: String,
        question: String,
        options: Vec<String>,
    },
    Edit {
        message_id: Seq,
        text: String,
    },
    Reaction {
        message_id: Seq,
        emoji: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Routing {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

const fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_content_round_trips_with_optional_fields_absent() {
        let json = r#"{"sender":"John","text":"hello"}"#;
        let InboundContent::Chat(chat) = InboundContent::parse("chat", json).expect("parse") else {
            panic!("expected chat content");
        };
        assert_eq!(chat.sender, "John");
        assert_eq!(chat.text, "hello");
        assert!(chat.attachments.is_empty());
        assert!(!chat.is_from_me);
        let back = serde_json::to_string(&chat).expect("serialize");
        let InboundContent::Chat(reparsed) = InboundContent::parse("chat", &back).expect("reparse")
        else {
            panic!("expected chat content");
        };
        assert_eq!(reparsed, chat);
    }

    #[test]
    fn task_content_defaults_wake_agent_to_true() {
        let InboundContent::Task(task) =
            InboundContent::parse("task", r#"{"prompt":"review PRs"}"#).expect("parse")
        else {
            panic!("expected task content");
        };
        assert!(task.wake_agent);
        assert_eq!(task.script, None);
    }

    #[test]
    fn webhook_and_system_parse_by_kind() {
        let webhook = InboundContent::parse(
            "webhook",
            r#"{"source":"github","event":"pull_request","payload":{"n":1}}"#,
        )
        .expect("parse webhook");
        assert!(matches!(webhook, InboundContent::Webhook(_)));

        let system = InboundContent::parse(
            "system",
            r#"{"action":"create_agent","status":"success","result":{"id":"ag-1"}}"#,
        )
        .expect("parse system");
        assert!(matches!(system, InboundContent::System(_)));
    }

    #[test]
    fn unknown_kind_falls_back_to_raw() {
        let raw = InboundContent::parse("chat-sdk", r#"{"anything":true}"#).expect("parse");
        let InboundContent::Raw(value) = raw else {
            panic!("expected raw content");
        };
        assert_eq!(value["anything"], Value::Bool(true));
    }

    #[test]
    fn malformed_payload_reports_the_kind() {
        let err = InboundContent::parse("chat", "not json").expect_err("must fail");
        let ContentError::Malformed { kind, .. } = err;
        assert_eq!(kind, "chat");
    }

    #[test]
    fn plain_outbound_text_round_trips() {
        let content = OutboundContent::from_text("LGTM");
        let json = serde_json::to_string(&content).expect("serialize");
        assert_eq!(json, r#"{"text":"LGTM"}"#);
        let back = OutboundContent::parse(&json).expect("parse");
        assert_eq!(back, content);
    }

    #[test]
    fn ask_question_operation_round_trips() {
        let json = r#"{"operation":{"type":"ask_question","question_id":"q1","title":"Deploy","question":"Go ahead?","options":["yes","no"]}}"#;
        let content = OutboundContent::parse(json).expect("parse");
        let Some(Operation::AskQuestion { ref options, .. }) = content.operation else {
            panic!("expected ask_question operation");
        };
        assert_eq!(options, &["yes", "no"]);
        let back = serde_json::to_string(&content).expect("serialize");
        let reparsed = OutboundContent::parse(&back).expect("reparse");
        assert_eq!(reparsed, content);
    }

    #[test]
    fn edit_operation_carries_a_seq_message_id() {
        let json = r#"{"operation":{"type":"edit","message_id":5,"text":"updated"}}"#;
        let content = OutboundContent::parse(json).expect("parse");
        let Some(Operation::Edit {
            message_id,
            ref text,
        }) = content.operation
        else {
            panic!("expected edit operation");
        };
        assert_eq!(message_id, Seq::new(5));
        assert_eq!(text, "updated");
    }

    #[test]
    fn operation_keys_do_not_leak_into_extra() {
        let json = r#"{"operation":{"type":"reaction","message_id":3,"emoji":"+1"},"custom":1}"#;
        let content = OutboundContent::parse(json).expect("parse");
        assert!(matches!(
            content.operation,
            Some(Operation::Reaction { .. })
        ));
        assert_eq!(content.extra.len(), 1);
        assert_eq!(content.extra["custom"], Value::from(1));
    }

    #[test]
    fn unknown_outbound_fields_are_preserved_in_extra() {
        let json = r#"{"text":"hi","custom_field":42}"#;
        let content = OutboundContent::parse(json).expect("parse");
        assert_eq!(content.extra["custom_field"], Value::from(42));
        let back = serde_json::to_string(&content).expect("serialize");
        let reparsed = OutboundContent::parse(&back).expect("reparse");
        assert_eq!(reparsed, content);
    }

    #[test]
    fn routing_defaults_to_all_none() {
        let routing = Routing::default();
        assert_eq!(routing.channel_type, None);
        assert_eq!(routing.platform_id, None);
        assert_eq!(routing.thread_id, None);
    }
}
