use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

pub const BASH_TIMEOUT: Duration = Duration::from_secs(120);
/// Keep this much of each end of long output; the middle is elided.
const HEAD_BYTES: usize = 6_000;
const TAIL_BYTES: usize = 6_000;

/// Runs a shell command in the group workspace and renders the outcome as a
/// tool-result string for the model. Never errors: failures (spawn, timeout,
/// non-zero exit) are part of the rendered result.
pub async fn bash(workspace: &Path, command: &str) -> String {
    let spawned = Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match spawned {
        Ok(child) => child,
        Err(err) => return format!("failed to run bash: {err}"),
    };

    // Dropping the future on timeout kills the child (kill_on_drop).
    match tokio::time::timeout(BASH_TIMEOUT, child.wait_with_output()).await {
        Err(_) => format!(
            "timed out after {}s — command killed",
            BASH_TIMEOUT.as_secs()
        ),
        Ok(Err(err)) => format!("failed to read command output: {err}"),
        Ok(Ok(output)) => render_result(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        ),
    }
}

/// Pure: exit line + truncated stdout + truncated stderr.
fn render_result(exit_code: Option<i32>, stdout: &str, stderr: &str) -> String {
    let mut parts = vec![match exit_code {
        Some(code) => format!("exit code: {code}"),
        None => "terminated by signal".to_owned(),
    }];
    if !stdout.trim().is_empty() {
        parts.push(truncate_middle(stdout.trim_end(), HEAD_BYTES, TAIL_BYTES));
    }
    if !stderr.trim().is_empty() {
        parts.push(format!(
            "stderr:\n{}",
            truncate_middle(stderr.trim_end(), HEAD_BYTES, TAIL_BYTES)
        ));
    }
    if stdout.trim().is_empty() && stderr.trim().is_empty() {
        parts.push("(no output)".to_owned());
    }
    parts.join("\n")
}

/// Pure: keeps the first `head` and last `tail` bytes (on char boundaries)
/// with an elision marker between — long output keeps both its start and end.
fn truncate_middle(text: &str, head: usize, tail: usize) -> String {
    if text.len() <= head + tail {
        return text.to_owned();
    }
    let head_end = floor_char_boundary(text, head);
    let tail_start = ceil_char_boundary(text, text.len() - tail);
    let elided = tail_start - head_end;
    format!(
        "{}\n[… {elided} bytes elided …]\n{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[tokio::test]
    async fn captures_stdout_and_exit_code() {
        let tmp = workspace();
        let result = bash(tmp.path(), "echo hello").await;
        assert_eq!(result, "exit code: 0\nhello");
    }

    #[tokio::test]
    async fn runs_in_the_workspace_directory() {
        let tmp = workspace();
        std::fs::write(tmp.path().join("marker.txt"), "found").expect("write");
        let result = bash(tmp.path(), "cat marker.txt").await;
        assert_eq!(result, "exit code: 0\nfound");
    }

    #[tokio::test]
    async fn reports_stderr_and_nonzero_exit() {
        let tmp = workspace();
        let result = bash(tmp.path(), "echo oops >&2; exit 3").await;
        assert_eq!(result, "exit code: 3\nstderr:\noops");
    }

    #[tokio::test]
    async fn empty_output_is_explicit() {
        let tmp = workspace();
        let result = bash(tmp.path(), "true").await;
        assert_eq!(result, "exit code: 0\n(no output)");
    }

    #[tokio::test]
    async fn long_output_keeps_head_and_tail() {
        let tmp = workspace();
        let result = bash(tmp.path(), "seq 1 20000").await;
        assert!(result.contains("bytes elided"));
        assert!(result.contains("\n1\n"), "head preserved");
        assert!(result.contains("20000"), "tail preserved");
    }

    #[test]
    fn truncate_middle_table() {
        let cases = [
            ("short", 10, 10, "short"),
            ("abcdefghij", 3, 3, "abc\n[… 4 bytes elided …]\nhij"),
        ];
        for (input, head, tail, expected) in cases {
            assert_eq!(truncate_middle(input, head, tail), expected, "{input:?}");
        }
        let multibyte = "ééééééééééé"; // 11 chars × 2 bytes
        let truncated = truncate_middle(multibyte, 3, 3);
        assert!(truncated.contains("elided"));
        assert!(!truncated.is_empty(), "must not panic on char boundaries");
    }

    #[test]
    fn render_result_table() {
        assert_eq!(render_result(Some(0), "", ""), "exit code: 0\n(no output)");
        assert_eq!(render_result(None, "x", ""), "terminated by signal\nx");
        assert_eq!(
            render_result(Some(1), "out", "err"),
            "exit code: 1\nout\nstderr:\nerr"
        );
    }
}
