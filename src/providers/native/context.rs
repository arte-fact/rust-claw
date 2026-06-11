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
    messages
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
