use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::protocol::content::{
    Attachment, ChatContent, InboundContent, SystemResult, TaskContent, WebhookContent,
};
use crate::session::InboundMessage;

/// Renders a batch of inbound messages into pi's XML prompt (§8.4). Routing
/// fields never appear — the agent sees content, sender, and local time only;
/// replies inherit routing from the row they answer.
#[must_use]
pub fn format_batch(messages: &[InboundMessage], timezone: &str) -> String {
    let tz = TimeZone::get(timezone).unwrap_or(TimeZone::UTC);
    let mut parts = vec![format!(
        "<context timezone=\"{}\" />",
        escape_attr(timezone)
    )];

    let mut chats = Vec::new();
    let mut others = Vec::new();
    for message in messages {
        match InboundContent::parse(&message.kind, &message.content) {
            Ok(InboundContent::Chat(chat)) => chats.push(format_chat(message, &chat, &tz)),
            Ok(InboundContent::Task(task)) => others.push(format_task(&task)),
            Ok(InboundContent::Webhook(hook)) => others.push(format_webhook(&hook)),
            Ok(InboundContent::System(result)) => others.push(format_system(&result)),
            Ok(InboundContent::Raw(value)) => others.push(format_raw(message, &value)),
            Err(_) => others.push(format_raw(message, &message.content)),
        }
    }
    if !chats.is_empty() {
        parts.push(chats.join("\n"));
    }
    parts.extend(others);
    parts.join("\n\n")
}

fn format_chat(message: &InboundMessage, chat: &ChatContent, tz: &TimeZone) -> String {
    let mut body = vec![escape_text(chat.text.trim())];
    if let Some(quoted) = &chat.quoted {
        body.push(format!(
            "<quoted_message from=\"{}\">{}</quoted_message>",
            escape_attr(&quoted.sender),
            escape_text(&quoted.text)
        ));
    }
    for attachment in &chat.attachments {
        body.push(format_attachment(attachment));
    }
    format!(
        "<message id=\"{}\" sender=\"{}\" time=\"{}\">\n{}\n</message>",
        message.seq,
        escape_attr(&chat.sender),
        local_time(&message.timestamp, tz),
        body.join("\n")
    )
}

fn format_attachment(attachment: &Attachment) -> String {
    let location = attachment
        .path
        .as_deref()
        .or(attachment.url.as_deref())
        .unwrap_or(&attachment.name);
    format!("[file available at: {location}]")
}

fn format_task(task: &TaskContent) -> String {
    let mut body = String::new();
    if let Some(script) = &task.script {
        body.push_str("Script:\n");
        body.push_str(&escape_text(script));
        body.push_str("\n\n");
    }
    body.push_str("Instructions:\n");
    body.push_str(&escape_text(&task.prompt));
    format!("<task>\n{body}\n</task>")
}

fn format_webhook(hook: &WebhookContent) -> String {
    format!(
        "<webhook source=\"{}\" event=\"{}\">\n{}\n</webhook>",
        escape_attr(&hook.source),
        escape_attr(&hook.event),
        hook.payload
    )
}

fn format_system(result: &SystemResult) -> String {
    format!(
        "<system_response action=\"{}\" status=\"{}\">\n{}\n</system_response>",
        escape_attr(&result.action),
        escape_attr(&result.status),
        result.result
    )
}

fn format_raw(message: &InboundMessage, value: &impl std::fmt::Display) -> String {
    format!(
        "<message kind=\"{}\">\n{value}\n</message>",
        escape_attr(&message.kind)
    )
}

/// UTC DB timestamp → local wall-clock for display; unparseable input passes through.
fn local_time(timestamp: &str, tz: &TimeZone) -> String {
    match timestamp.parse::<Timestamp>() {
        Ok(stamp) => stamp
            .to_zoned(tz.clone())
            .strftime("%Y-%m-%d %H:%M")
            .to_string(),
        Err(_) => timestamp.to_owned(),
    }
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::content::Routing;
    use crate::protocol::ids::MessageInId;
    use crate::protocol::message::{MessageStatus, Seq};

    fn message(seq: i64, kind: &str, content: &str) -> InboundMessage {
        InboundMessage {
            id: MessageInId::new(format!("in-{seq}")),
            seq: Seq::new(seq),
            kind: kind.to_owned(),
            timestamp: "2026-06-11T16:05:00Z".to_owned(),
            status: MessageStatus::Pending,
            process_after: None,
            recurrence: None,
            series_id: None,
            tries: 0,
            trigger: true,
            // Deliberately populated to prove routing is stripped from the output.
            routing: Routing {
                channel_type: Some("web".to_owned()),
                platform_id: Some("secret-platform-id".to_owned()),
                thread_id: Some("secret-thread".to_owned()),
            },
            content: content.to_owned(),
            source_session_id: None,
        }
    }

    #[test]
    fn header_carries_the_timezone_and_routing_is_never_emitted() {
        let batch = vec![message(0, "chat", r#"{"sender":"John","text":"hello"}"#)];
        let output = format_batch(&batch, "UTC");
        assert!(output.starts_with("<context timezone=\"UTC\" />"));
        assert!(!output.contains("secret-platform-id"));
        assert!(!output.contains("secret-thread"));
    }

    #[test]
    fn chat_message_renders_with_local_time_quote_and_attachment() {
        let content = r#"{
            "sender": "John",
            "text": "check this PR",
            "attachments": [{"name":"data.xlsx","path":"/data/sessions/ag/s/inbox/in-0/data.xlsx"}],
            "quoted": {"sender":"Jane","text":"did you see the feedback?"}
        }"#;
        let output = format_batch(&[message(0, "chat", content)], "America/Los_Angeles");
        let expected = "<context timezone=\"America/Los_Angeles\" />\n\n\
            <message id=\"0\" sender=\"John\" time=\"2026-06-11 09:05\">\n\
            check this PR\n\
            <quoted_message from=\"Jane\">did you see the feedback?</quoted_message>\n\
            [file available at: /data/sessions/ag/s/inbox/in-0/data.xlsx]\n\
            </message>";
        assert_eq!(output, expected);
    }

    #[test]
    fn multiple_chats_group_under_a_single_block() {
        let batch = vec![
            message(0, "chat", r#"{"sender":"A","text":"first"}"#),
            message(2, "chat", r#"{"sender":"B","text":"second"}"#),
        ];
        let output = format_batch(&batch, "UTC");
        assert_eq!(output.matches("<message ").count(), 2);
        // One blank line after the header, none between the grouped messages.
        assert_eq!(output.matches("\n\n").count(), 1);
        assert!(
            output.find("first").unwrap() < output.find("second").unwrap(),
            "messages keep batch order"
        );
    }

    #[test]
    fn task_webhook_and_system_each_render_their_own_block() {
        let task = format_batch(
            &[message(
                0,
                "task",
                r#"{"prompt":"review open PRs","script":"./pre.sh"}"#,
            )],
            "UTC",
        );
        assert!(task.contains("<task>"));
        assert!(task.contains("Script:\n./pre.sh"));
        assert!(task.contains("Instructions:\nreview open PRs"));

        let webhook = format_batch(
            &[message(
                0,
                "webhook",
                r#"{"source":"github","event":"pull_request","payload":{"number":7}}"#,
            )],
            "UTC",
        );
        assert!(webhook.contains("<webhook source=\"github\" event=\"pull_request\">"));
        assert!(webhook.contains("\"number\":7"));

        let system = format_batch(
            &[message(
                0,
                "system",
                r#"{"action":"create_agent","status":"success","result":{"id":"ag-1"}}"#,
            )],
            "UTC",
        );
        assert!(system.contains("<system_response action=\"create_agent\" status=\"success\">"));
    }

    #[test]
    fn text_with_markup_is_escaped() {
        let output = format_batch(
            &[message(
                0,
                "chat",
                r#"{"sender":"A","text":"1 < 2 & <b>x</b>"}"#,
            )],
            "UTC",
        );
        assert!(output.contains("1 &lt; 2 &amp; &lt;b&gt;x&lt;/b&gt;"));
    }

    #[test]
    fn unparseable_content_falls_back_to_a_kind_tagged_block() {
        let output = format_batch(&[message(0, "chat", "not json")], "UTC");
        assert!(output.contains("<message kind=\"chat\">"));
        assert!(output.contains("not json"));
    }
}
