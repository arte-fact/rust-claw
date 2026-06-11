pub mod rpc;

use std::ffi::{OsStr, OsString};

use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{ActiveRun, AgentProvider, ProviderError, QueryInput};

const FOLLOW_UP_CHANNEL_CAPACITY: usize = 8;

/// Drives the pi coding agent over its RPC mode (§8.4). pi owns its own LLM
/// config and session persistence (via `--session-dir`); claw only feeds
/// prompts and reads events.
pub struct PiProvider {
    program: OsString,
}

impl PiProvider {
    #[must_use]
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// `CLAW_PI_BIN` overrides the binary (tests point it at a stub); default `pi`.
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(std::env::var_os("CLAW_PI_BIN").unwrap_or_else(|| OsString::from("pi")))
    }
}

impl AgentProvider for PiProvider {
    fn start(&self, input: QueryInput) -> Result<ActiveRun, ProviderError> {
        let command = build_command(&self.program, &input);
        let (input_tx, input_rx) = mpsc::channel(FOLLOW_UP_CHANNEL_CAPACITY);
        let abort = CancellationToken::new();
        let events = rpc::spawn(command, input.prompt, input_rx, abort.clone())
            .map_err(|err| ProviderError::Spawn(err.to_string()))?;
        Ok(ActiveRun {
            input: input_tx,
            events,
            abort,
        })
    }
}

/// pi reads its session tree from `--session-dir` (self-resuming, no token to
/// persist) and runs in the agent group workspace.
fn build_command(program: &OsStr, input: &QueryInput) -> Command {
    let mut command = Command::new(program);
    command
        .arg("--mode")
        .arg("rpc")
        .arg("--session-dir")
        .arg(input.session_dir.join("pi"))
        .current_dir(&input.cwd);
    if let Some(model) = &input.model {
        command.arg("--model").arg(model);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn query() -> QueryInput {
        QueryInput {
            prompt: "hi".to_owned(),
            cwd: PathBuf::from("/data/groups/chat"),
            session_dir: PathBuf::from("/data/sessions/ag/sess"),
            model: Some("qwen3.6-dense".to_owned()),
            system_context: None,
            inference: None,
        }
    }

    fn args_of(command: &Command) -> Vec<String> {
        command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn command_requests_rpc_mode_with_the_session_dir_and_model() {
        let command = build_command(OsStr::new("pi"), &query());
        let args = args_of(&command);
        assert_eq!(
            args,
            vec![
                "--mode",
                "rpc",
                "--session-dir",
                "/data/sessions/ag/sess/pi",
                "--model",
                "qwen3.6-dense",
            ]
        );
        assert_eq!(
            command.as_std().get_current_dir(),
            Some(std::path::Path::new("/data/groups/chat"))
        );
    }

    #[test]
    fn model_flag_is_omitted_when_unset() {
        let mut input = query();
        input.model = None;
        let args = args_of(&build_command(OsStr::new("pi"), &input));
        assert!(!args.iter().any(|arg| arg == "--model"));
    }

    #[tokio::test]
    async fn start_runs_the_configured_binary_and_streams_a_turn() {
        use crate::providers::ProviderEvent;
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let script = tmp.path().join("fake-pi.sh");
        std::fs::write(
            &script,
            "#!/usr/bin/env bash\nread -r _line\nprintf '%s\\n' \
             '{\"type\":\"agent_end\",\"messages\":[{\"role\":\"assistant\",\"content\":\"pi says hi\"}]}'\n",
        )
        .expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let provider = PiProvider::new(script);
        let mut input = query();
        input.cwd = tmp.path().to_path_buf();
        input.session_dir = tmp.path().to_path_buf();
        let mut run = provider.start(input).expect("start");

        let text = loop {
            match run.events.recv().await {
                Some(ProviderEvent::TurnEnd { text }) => break text,
                Some(_) => {}
                None => panic!("stream ended without a turn"),
            }
        };
        assert_eq!(text.as_deref(), Some("pi says hi"));
    }
}
