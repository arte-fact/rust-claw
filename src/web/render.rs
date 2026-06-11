use askama::Template;
use pulldown_cmark::{Event, Options, Parser, html};

use crate::db::web_messages::{Direction, MessageRowKind, WebMessage};

#[derive(Template)]
#[template(path = "message.html")]
struct MessageItem {
    id: i64,
    direction: &'static str,
    sender: String,
    time: String,
    body_html: String,
}

#[derive(Template)]
#[template(path = "question.html")]
struct QuestionCard {
    id: i64,
    question_id: String,
    sender: String,
    time: String,
    question_html: String,
    options: Vec<String>,
    answer: Option<String>,
}

/// One transcript item — used by both the full page render and SSE fragments.
/// Question rows render as an interactive (or, once answered, collapsed) card.
#[must_use]
pub fn message_html(message: &WebMessage) -> String {
    match message.kind {
        MessageRowKind::Question => question_html(message),
        MessageRowKind::Chat => chat_html(message),
    }
}

fn chat_html(message: &WebMessage) -> String {
    let body_html = match message.direction {
        Direction::Out => render_markdown(&message.body),
        Direction::In => escaped_paragraphs(&message.body),
    };
    let item = MessageItem {
        id: message.id,
        direction: message.direction.as_str(),
        sender: message.sender.clone(),
        time: clock_time(&message.created_at),
        body_html,
    };
    item.render().unwrap_or_else(|error| {
        tracing::error!(%error, "message template render failed");
        String::new()
    })
}

fn question_html(message: &WebMessage) -> String {
    let card = QuestionCard {
        id: message.id,
        question_id: message.question_id.clone().unwrap_or_default(),
        sender: message.sender.clone(),
        time: clock_time(&message.created_at),
        question_html: escape_html(&message.body),
        options: message.options.clone(),
        answer: message.answer.clone(),
    };
    card.render().unwrap_or_else(|error| {
        tracing::error!(%error, "question template render failed");
        String::new()
    })
}

/// SSE `message` payload: which chat it belongs to plus the rendered fragment.
#[must_use]
pub fn message_event_payload(platform_id: &str, message: &WebMessage) -> String {
    serde_json::json!({
        "chat": platform_id,
        "id": message.id,
        "html": message_html(message),
    })
    .to_string()
}

/// SSE `message_update` payload: replaces an existing transcript element in place
/// (e.g. a question card collapsing once answered).
#[must_use]
pub fn message_update_payload(message: &WebMessage) -> String {
    serde_json::json!({
        "id": message.id,
        "html": message_html(message),
    })
    .to_string()
}

/// Agent output is markdown; raw HTML inside it is neutralized to text.
fn render_markdown(source: &str) -> String {
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let events = Parser::new_ext(source, options).map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, events);
    out
}

/// User text is shown verbatim: escaped, newline-aware, never interpreted.
fn escaped_paragraphs(source: &str) -> String {
    let escaped = escape_html(source).replace('\n', "<br>");
    format!("<p>{escaped}</p>")
}

fn escape_html(source: &str) -> String {
    source
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// "2026-06-11T09:05:45.792Z" → "09:05"; anything malformed renders empty.
fn clock_time(timestamp: &str) -> String {
    timestamp.get(11..16).unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ids::MessagingGroupId;

    fn message(direction: Direction, body: &str) -> WebMessage {
        WebMessage {
            id: 7,
            messaging_group_id: MessagingGroupId::new("mg-1"),
            direction,
            sender: "andy".to_owned(),
            body: body.to_owned(),
            message_out_id: None,
            created_at: "2026-06-11T09:05:45.792Z".to_owned(),
            kind: MessageRowKind::Chat,
            question_id: None,
            options: Vec::new(),
            answer: None,
        }
    }

    fn question(answer: Option<&str>) -> WebMessage {
        WebMessage {
            kind: MessageRowKind::Question,
            question_id: Some("q-7".to_owned()),
            options: vec!["ship it".to_owned(), "hold".to_owned()],
            answer: answer.map(str::to_owned),
            ..message(Direction::Out, "Deploy <prod> now?")
        }
    }

    #[test]
    fn agent_markdown_is_rendered() {
        let html = message_html(&message(Direction::Out, "**bold** and `code`"));
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<code>code</code>"));
        assert!(html.contains("msg--out"));
        assert!(html.contains("09:05"));
    }

    #[test]
    fn raw_html_in_agent_output_is_neutralized() {
        let html = message_html(&message(Direction::Out, "<script>alert(1)</script>hi"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn user_text_is_escaped_and_newlines_become_breaks() {
        let html = message_html(&message(Direction::In, "a < b\nnext <i>line</i>"));
        assert!(html.contains("a &lt; b<br>next &lt;i&gt;line&lt;/i&gt;"));
        assert!(html.contains("msg--in"));
    }

    #[test]
    fn event_payload_carries_chat_id_and_fragment() {
        let payload = message_event_payload("chat-9", &message(Direction::Out, "hi"));
        let value: serde_json::Value = serde_json::from_str(&payload).expect("json");
        assert_eq!(value["chat"], "chat-9");
        assert_eq!(value["id"], 7);
        assert!(value["html"].as_str().expect("html").contains("hi"));
    }

    #[test]
    fn open_question_renders_buttons_for_each_option() {
        let html = message_html(&question(None));
        assert!(html.contains("msg--question"));
        assert!(html.contains("data-question=\"q-7\""));
        assert!(html.contains("data-option=\"ship it\""));
        assert!(html.contains("data-option=\"hold\""));
        // The question text is escaped, never interpreted as HTML.
        assert!(html.contains("Deploy &lt;prod&gt; now?"));
        assert!(!html.contains("qcard--answered"));
    }

    #[test]
    fn answered_question_collapses_to_the_choice() {
        let html = message_html(&question(Some("ship it")));
        assert!(html.contains("qcard--answered"));
        assert!(html.contains("ship it"));
        // No interactive buttons remain once answered.
        assert!(!html.contains("data-option="));
    }
}
