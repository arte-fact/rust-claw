use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{ActiveRun, AgentProvider, ProviderError, ProviderEvent, QueryInput};

/// Test provider: answers every prompt and follow-up with the text it received.
pub struct EchoProvider;

impl AgentProvider for EchoProvider {
    fn start(&self, input: QueryInput) -> Result<ActiveRun, ProviderError> {
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (event_tx, event_rx) = mpsc::channel::<ProviderEvent>(16);
        let abort = CancellationToken::new();
        let run_abort = abort.clone();

        tokio::spawn(async move {
            let mut pending = Some(input.prompt);
            while let Some(prompt) = pending.take() {
                let turn = ProviderEvent::TurnEnd { text: Some(prompt) };
                tokio::select! {
                    () = run_abort.cancelled() => break,
                    result = event_tx.send(turn) => {
                        if result.is_err() {
                            break;
                        }
                    }
                }
                tokio::select! {
                    () = run_abort.cancelled() => break,
                    next = input_rx.recv() => pending = next,
                }
            }
        });

        Ok(ActiveRun {
            input: input_tx,
            events: event_rx,
            abort,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn query(prompt: &str) -> QueryInput {
        QueryInput {
            prompt: prompt.to_owned(),
            cwd: PathBuf::from("."),
            session_dir: PathBuf::from("."),
            model: None,
            system_context: None,
        }
    }

    #[tokio::test]
    async fn echoes_the_prompt_and_each_follow_up() {
        let mut run = EchoProvider.start(query("hello")).expect("start");
        assert_eq!(
            run.events.recv().await,
            Some(ProviderEvent::TurnEnd {
                text: Some("hello".to_owned())
            })
        );
        run.input.send("again".to_owned()).await.expect("push");
        assert_eq!(
            run.events.recv().await,
            Some(ProviderEvent::TurnEnd {
                text: Some("again".to_owned())
            })
        );
        drop(run.input);
        assert_eq!(run.events.recv().await, None);
    }

    #[tokio::test]
    async fn abort_ends_the_event_stream() {
        let mut run = EchoProvider.start(query("hello")).expect("start");
        run.abort.cancel();
        while run.events.recv().await.is_some() {}
    }
}
