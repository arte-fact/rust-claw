use askama::Template;
use pulldown_cmark::{Event, Options, Parser, html};

use crate::db::web_messages::{Direction, WebMessage};

#[derive(Template)]
#[template(path = "message.html")]
struct MessageItem {
    id: i64,
    direction: &'static str,
    sender: String,
    time: String,
    body_html: String,
}

/// One transcript item — used by both the full page render and SSE fragments.
#[must_use]
pub fn message_html(message: &WebMessage) -> String {
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
}
