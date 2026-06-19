use crate::protocol::content::{InboundContent, OutboundContent};
use crate::session::TranscriptEntry;

use super::client::{ChatMessage, Role};

/// Crude but tokenizer-free: ~4 characters per token, biased low to stay safe.
#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

// KNOWN LIMITATION (PLAN 4.7): the transcript holds chat in/out only. A turn's
// intermediate tool rounds (bash/edit calls + results) live in run_turn's local
// message list and are NOT persisted, so a *later* user message can't see them.
// Within one turn the agent has full tool context; across turns it must re-derive
// state (re-run `ls`/`read`) or rely on what it summarized into its replies. A
// fuller fix — persisting tool rounds — is a backlog item, deferred until this
// proves insufficient in practice.

/// Builds the chat-completion message list: system prompt first, then the newest
/// transcript entries that fit the token budget, oldest of the kept ones first.
#[must_use]
pub fn build_messages(
    system_prompt: Option<&str>,
    transcript: &[TranscriptEntry],
    token_budget: usize,
) -> Vec<ChatMessage> {
    let mut remaining = token_budget;
    let mut kept_newest_first = Vec::new();
    for entry in transcript.iter().rev() {
        let Some(message) = to_chat_message(entry) else {
            continue;
        };
        let cost = estimate_tokens(message.content.as_deref().unwrap_or_default());
        if cost > remaining {
            break;
        }
        remaining -= cost;
        kept_newest_first.push(message);
    }

    let mut messages = Vec::with_capacity(kept_newest_first.len() + 1);
    if let Some(prompt) = system_prompt {
        messages.push(ChatMessage::new(Role::System, prompt));
    }
    messages.extend(kept_newest_first.into_iter().rev());
    drop_trailing_assistant(coalesce_adjacent_roles(messages))
}

/// Drop trailing assistant message(s) so the request ends with a user (or tool)
/// turn. A chat-completion endpoint rejects a list ending in the assistant's own
/// message — you'd be asking it to *continue* that message, not answer the user.
/// This happens whenever a turn fires with the transcript ending in the agent's
/// reply (e.g. a re-engaged session, or a turn that already answered). `coalesce`
/// alone is not enough: merging two trailing replies still leaves one trailing
/// assistant, which the endpoint still refuses.
fn drop_trailing_assistant(mut messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    while matches!(messages.last(), Some(message) if message.role == Role::Assistant) {
        messages.pop();
    }
    messages
}

/// Merges consecutive same-role messages into one. The transcript can hold two
/// assistant turns in a row (the agent sent two replies, or a reply was rewritten
/// by a retry) or two user turns (batched inbounds); a chat-completion endpoint
/// then rejects the request — "cannot have 2 or more assistant messages at the
/// end of the list" — so the run never completes. Joining their text keeps the
/// roles strictly alternating after the system prompt.
fn coalesce_adjacent_roles(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    for message in messages {
        match out.last_mut() {
            Some(last) if last.role == message.role => {
                let joined = match (last.content.take(), message.content) {
                    (Some(a), Some(b)) => Some(format!("{a}\n{b}")),
                    (a, b) => a.or(b),
                };
                last.content = joined;
            }
            _ => out.push(message),
        }
    }
    out
}

/// The agent sees content only — routing never appears. Non-chat kinds are
/// labeled so the model knows what it is looking at.
fn to_chat_message(entry: &TranscriptEntry) -> Option<ChatMessage> {
    if entry.inbound {
        let text = match InboundContent::parse(&entry.kind, &entry.content) {
            Ok(InboundContent::Chat(chat)) => {
                if chat.sender.is_empty() {
                    chat.text
                } else {
                    format!("[{}] {}", chat.sender, chat.text)
                }
            }
            Ok(InboundContent::Task(task)) => format!("[scheduled task] {}", task.prompt),
            Ok(InboundContent::Webhook(webhook)) => {
                format!(
                    "[webhook {}/{}] {}",
                    webhook.source, webhook.event, webhook.payload
                )
            }
            Ok(InboundContent::System(result)) => {
                format!(
                    "[system {} {}] {}",
                    result.action, result.status, result.result
                )
            }
            Ok(InboundContent::Raw(value)) => value.to_string(),
            Err(_) => entry.content.clone(),
        };
        Some(ChatMessage::new(Role::User, text))
    } else {
        let text = OutboundContent::parse(&entry.content)
            .ok()
            .and_then(|content| content.text)?;
        Some(ChatMessage::new(Role::Assistant, text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::message::Seq;

    fn inbound(seq: i64, sender: &str, text: &str) -> TranscriptEntry {
        TranscriptEntry {
            seq: Seq::new(seq),
            inbound: true,
            kind: "chat".to_owned(),
            content: format!("{{\"sender\":\"{sender}\",\"text\":\"{text}\"}}"),
        }
    }

    fn outbound(seq: i64, text: &str) -> TranscriptEntry {
        TranscriptEntry {
            seq: Seq::new(seq),
            inbound: false,
            kind: "chat".to_owned(),
            content: format!("{{\"text\":\"{text}\"}}"),
        }
    }

    #[test]
    fn conversation_maps_to_roles_with_sender_labels() {
        let transcript = vec![
            inbound(0, "you", "hello"),
            outbound(1, "hi there"),
            inbound(2, "you", "how are you?"),
        ];
        let messages = build_messages(Some("be concise"), &transcript, 10_000);
        let shape: Vec<(Role, &str)> = messages
            .iter()
            .map(|message| (message.role, message.content.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(
            shape,
            vec![
                (Role::System, "be concise"),
                (Role::User, "[you] hello"),
                (Role::Assistant, "hi there"),
                (Role::User, "[you] how are you?"),
            ]
        );
    }

    #[test]
    fn budget_drops_oldest_messages_first_and_keeps_the_system_prompt() {
        let transcript = vec![
            inbound(0, "you", "a very old message that should be dropped first"),
            outbound(1, "an old reply that should also fall out of the window"),
            inbound(2, "you", "newest"),
        ];
        let newest_cost = estimate_tokens("[you] newest");
        let messages = build_messages(Some("sys"), &transcript, newest_cost);
        let shape: Vec<(Role, &str)> = messages
            .iter()
            .map(|message| (message.role, message.content.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(
            shape,
            vec![(Role::System, "sys"), (Role::User, "[you] newest")]
        );
    }

    #[test]
    fn zero_budget_yields_only_the_system_prompt() {
        let transcript = vec![inbound(0, "you", "hello")];
        let messages = build_messages(Some("sys"), &transcript, 0);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::System);
    }

    #[test]
    fn unparseable_outbound_entries_are_skipped() {
        let broken = TranscriptEntry {
            seq: Seq::new(1),
            inbound: false,
            kind: "chat".to_owned(),
            content: "not json".to_owned(),
        };
        let messages = build_messages(None, &[inbound(0, "you", "hi"), broken], 10_000);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);
    }

    #[test]
    fn consecutive_assistant_replies_are_merged() {
        // Two outbound replies in a row (a rewrite/duplicate or a double-send) must
        // not become two adjacent assistant messages — the API needs alternating
        // roles. A later user turn keeps the merged reply off the end of the list.
        let transcript = vec![
            inbound(0, "you", "hi"),
            outbound(1, "first reply"),
            outbound(3, "second reply"),
            inbound(4, "you", "still there?"),
        ];
        let messages = build_messages(Some("sys"), &transcript, 10_000);
        let shape: Vec<(Role, &str)> = messages
            .iter()
            .map(|m| (m.role, m.content.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(
            shape,
            vec![
                (Role::System, "sys"),
                (Role::User, "[you] hi"),
                (Role::Assistant, "first reply\nsecond reply"),
                (Role::User, "[you] still there?"),
            ]
        );
    }

    #[test]
    fn a_trailing_assistant_reply_is_dropped() {
        // A turn can fire with the transcript ending in the agent's own reply (a
        // re-engaged session, or a turn that already answered then ran again). The
        // built list must still end with the user's turn, never the assistant's —
        // the endpoint rejects "2 or more assistant messages at the end of the list".
        let transcript = vec![
            inbound(0, "you", "hi"),
            outbound(1, "first reply"),
            outbound(3, "second reply"),
        ];
        let messages = build_messages(Some("sys"), &transcript, 10_000);
        let shape: Vec<(Role, &str)> = messages
            .iter()
            .map(|m| (m.role, m.content.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(shape, vec![(Role::System, "sys"), (Role::User, "[you] hi")]);
        assert_ne!(messages.last().expect("non-empty").role, Role::Assistant);
    }

    #[test]
    fn consecutive_user_messages_are_merged() {
        let transcript = vec![
            inbound(0, "you", "one"),
            inbound(2, "you", "two"),
            outbound(3, "ok"),
            inbound(4, "you", "more"),
        ];
        let messages = build_messages(None, &transcript, 10_000);
        let shape: Vec<(Role, &str)> = messages
            .iter()
            .map(|m| (m.role, m.content.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(
            shape,
            vec![
                (Role::User, "[you] one\n[you] two"),
                (Role::Assistant, "ok"),
                (Role::User, "[you] more"),
            ]
        );
    }

    #[test]
    fn task_and_webhook_kinds_are_labeled() {
        let task = TranscriptEntry {
            seq: Seq::new(0),
            inbound: true,
            kind: "task".to_owned(),
            content: "{\"prompt\":\"daily briefing\"}".to_owned(),
        };
        let messages = build_messages(None, &[task], 10_000);
        assert_eq!(
            messages[0].content.as_deref(),
            Some("[scheduled task] daily briefing")
        );
    }
}
