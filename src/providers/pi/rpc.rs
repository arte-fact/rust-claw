use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::providers::ProviderEvent;

/// Grace between graceful abort (RPC `abort` + SIGTERM) and a forced SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);
const EVENT_CHANNEL_CAPACITY: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum PiError {
    #[error("failed to spawn pi: {0}")]
    Spawn(String),
}

#[must_use]
pub fn prompt_line(message: &str) -> String {
    line(&json!({ "type": "prompt", "message": message }))
}

#[must_use]
pub fn follow_up_line(message: &str) -> String {
    line(&json!({ "type": "follow_up", "message": message }))
}

#[must_use]
pub fn abort_line() -> String {
    line(&json!({ "type": "abort" }))
}

fn line(value: &Value) -> String {
    let mut encoded = value.to_string();
    encoded.push('\n');
    encoded
}

/// Maps one pi RPC stdout event to a provider event. `None` = recognized but
/// not worth surfacing (command acks, queue updates). Every recognized event
/// other than those counts as liveness for the watchdog.
#[must_use]
pub fn translate(raw_line: &str) -> Option<ProviderEvent> {
    let value: Value = serde_json::from_str(raw_line).ok()?;
    match value.get("type").and_then(Value::as_str)? {
        "agent_end" => Some(ProviderEvent::TurnEnd {
            text: value.get("messages").and_then(final_assistant_text),
        }),
        "tool_execution_start" => {
            let tool = value
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            Some(ProviderEvent::Progress {
                message: format!("running {tool}"),
            })
        }
        "error" => Some(ProviderEvent::Error {
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("pi reported an error")
                .to_owned(),
            retryable: false,
        }),
        "response" | "queue_update" => None,
        _ => Some(ProviderEvent::Activity),
    }
}

/// Pulls the last assistant message's text out of `agent_end.messages`, tolerant
/// of both string content and an array of `{type:"text", text}` blocks.
fn final_assistant_text(messages: &Value) -> Option<String> {
    let assistant = messages
        .as_array()?
        .iter()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))?;
    let content = assistant.get("content")?;
    if let Some(text) = content.as_str() {
        return non_empty(text);
    }
    if let Some(blocks) = content.as_array() {
        let joined: String = blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect();
        return non_empty(&joined);
    }
    None
}

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Spawns pi, sends the initial prompt, forwards follow-ups, and streams
/// translated events. Cancelling `abort` runs the graceful→forced shutdown
/// ladder. The returned receiver closes when pi's stdout ends.
pub fn spawn(
    mut command: Command,
    prompt: String,
    mut follow_ups: mpsc::Receiver<String>,
    abort: CancellationToken,
) -> Result<mpsc::Receiver<ProviderEvent>, PiError> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|err| PiError::Spawn(err.to_string()))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| PiError::Spawn("pi stdin not captured".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| PiError::Spawn("pi stdout not captured".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| PiError::Spawn("pi stderr not captured".to_owned()))?;

    let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<String>(EVENT_CHANNEL_CAPACITY);

    let feed_tx = cmd_tx.clone();
    tokio::spawn(async move {
        if feed_tx.send(prompt_line(&prompt)).await.is_err() {
            return;
        }
        while let Some(message) = follow_ups.recv().await {
            if feed_tx.send(follow_up_line(&message)).await.is_err() {
                break;
            }
        }
    });

    tokio::spawn(async move {
        while let Some(out) = cmd_rx.recv().await {
            if stdin.write_all(out.as_bytes()).await.is_err() {
                break;
            }
            let _ = stdin.flush().await;
        }
    });

    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(raw)) = lines.next_line().await {
            if let Some(event) = translate(&raw)
                && event_tx.send(event).await.is_err()
            {
                break;
            }
        }
    });

    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(raw)) = lines.next_line().await {
            tracing::warn!(target: "pi", message = %raw, "pi stderr");
        }
    });

    tokio::spawn(async move {
        tokio::select! {
            _ = child.wait() => return,
            () = abort.cancelled() => {}
        }
        let _ = cmd_tx.send(abort_line()).await;
        if let Some(pid) = child.id() {
            send_sigterm(pid).await;
        }
        if timeout(KILL_GRACE, child.wait()).await.is_err() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    });

    Ok(event_rx)
}

async fn send_sigterm(pid: u32) {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_are_newline_terminated_json() {
        assert_eq!(
            prompt_line("hi"),
            "{\"message\":\"hi\",\"type\":\"prompt\"}\n"
        );
        assert_eq!(
            follow_up_line("more"),
            "{\"message\":\"more\",\"type\":\"follow_up\"}\n"
        );
        assert_eq!(abort_line(), "{\"type\":\"abort\"}\n");
    }

    #[test]
    fn agent_end_yields_turn_end_with_string_content() {
        let event = translate(
            r#"{"type":"agent_end","messages":[
                {"role":"user","content":"hi"},
                {"role":"assistant","content":"all done"}
            ]}"#,
        );
        assert_eq!(
            event,
            Some(ProviderEvent::TurnEnd {
                text: Some("all done".to_owned())
            })
        );
    }

    #[test]
    fn agent_end_extracts_text_from_content_blocks() {
        let event = translate(
            r#"{"type":"agent_end","messages":[
                {"role":"assistant","content":[
                    {"type":"thinking","text":"hmm"},
                    {"type":"text","text":"part one "},
                    {"type":"text","text":"part two"}
                ]}
            ]}"#,
        );
        assert_eq!(
            event,
            Some(ProviderEvent::TurnEnd {
                text: Some("part one part two".to_owned())
            })
        );
    }

    #[test]
    fn agent_end_without_assistant_text_is_a_textless_turn_end() {
        let event =
            translate(r#"{"type":"agent_end","messages":[{"role":"user","content":"hi"}]}"#);
        assert_eq!(event, Some(ProviderEvent::TurnEnd { text: None }));
    }

    #[test]
    fn tool_execution_start_becomes_progress() {
        let event = translate(
            r#"{"type":"tool_execution_start","toolCallId":"c1","toolName":"bash","args":{}}"#,
        );
        assert_eq!(
            event,
            Some(ProviderEvent::Progress {
                message: "running bash".to_owned()
            })
        );
    }

    #[test]
    fn streaming_and_lifecycle_events_count_as_activity() {
        for raw in [
            r#"{"type":"agent_start"}"#,
            r#"{"type":"turn_start"}"#,
            r#"{"type":"message_update","message":{}}"#,
            r#"{"type":"tool_execution_end","toolCallId":"c1","result":{},"isError":false}"#,
        ] {
            assert_eq!(translate(raw), Some(ProviderEvent::Activity), "{raw}");
        }
    }

    #[test]
    fn command_acks_and_queue_updates_are_dropped() {
        assert_eq!(
            translate(r#"{"type":"response","command":"prompt","success":true}"#),
            None
        );
        assert_eq!(translate(r#"{"type":"queue_update","steering":[]}"#), None);
    }

    #[test]
    fn errors_translate_to_a_terminal_error_event() {
        let event = translate(r#"{"type":"error","message":"context overflow"}"#);
        assert_eq!(
            event,
            Some(ProviderEvent::Error {
                message: "context overflow".to_owned(),
                retryable: false,
            })
        );
    }

    #[test]
    fn malformed_lines_are_ignored() {
        assert_eq!(translate("not json"), None);
        assert_eq!(translate("{}"), None);
    }

    /// A fake pi: echoes each received command line to `$1`, then emits a
    /// start / tool / agent_end sequence per line.
    const FAKE_PI: &str = r#"#!/usr/bin/env bash
capture="$1"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$capture"
  printf '%s\n' '{"type":"agent_start"}'
  printf '%s\n' '{"type":"tool_execution_start","toolName":"bash","args":{}}'
  printf '%s\n' '{"type":"agent_end","messages":[{"role":"assistant","content":"hello from fake pi"}]}'
done
"#;

    async fn next_turn(events: &mut mpsc::Receiver<ProviderEvent>) -> Option<String> {
        loop {
            match events.recv().await {
                Some(ProviderEvent::TurnEnd { text }) => return Some(text.unwrap_or_default()),
                Some(_) => {}
                None => return None,
            }
        }
    }

    #[tokio::test]
    async fn spawn_drives_a_prompt_a_follow_up_and_aborts_cleanly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = tmp.path().join("fake-pi.sh");
        let capture = tmp.path().join("capture.txt");
        std::fs::write(&script, FAKE_PI).expect("write script");

        let mut command = Command::new("bash");
        command.arg(&script).arg(&capture);
        let (follow_up_tx, follow_up_rx) = mpsc::channel(4);
        let abort = CancellationToken::new();
        let mut events = spawn(
            command,
            "the-prompt".to_owned(),
            follow_up_rx,
            abort.clone(),
        )
        .expect("spawn");

        assert_eq!(
            next_turn(&mut events).await.as_deref(),
            Some("hello from fake pi")
        );

        follow_up_tx
            .send("second-prompt".to_owned())
            .await
            .expect("send follow-up");
        assert_eq!(
            next_turn(&mut events).await.as_deref(),
            Some("hello from fake pi")
        );

        let captured = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&capture)
                    && text.lines().count() >= 2
                {
                    return text;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("capture file");
        let lines: Vec<&str> = captured.lines().collect();
        assert!(lines[0].contains("the-prompt"), "{}", lines[0]);
        assert!(lines[1].contains("second-prompt"), "{}", lines[1]);

        abort.cancel();
        let closed = tokio::time::timeout(Duration::from_secs(5), async {
            while events.recv().await.is_some() {}
        })
        .await;
        assert!(closed.is_ok(), "events channel must close after abort");
    }

    #[tokio::test]
    async fn spawn_surfaces_a_spawn_failure() {
        let command = Command::new("/no/such/pi/binary");
        let (_tx, rx) = mpsc::channel(1);
        let result = spawn(command, "hi".to_owned(), rx, CancellationToken::new());
        assert!(result.is_err());
    }
}
