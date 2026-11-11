//! Running external tools (yt-dlp, ffmpeg, ffprobe, fpcalc) with a deadline and cancellation.

use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub struct Output {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("{0} not found or not executable: {1}")]
    Spawn(String, std::io::Error),
    #[error("{0} timed out")]
    Timeout(String),
    #[error("cancelled")]
    Cancelled,
}

/// Run `program args...`, killing it when `timeout` passes or `cancel` fires.
pub async fn run(program: &str, args: &[String], timeout: Duration, cancel: Option<&CancellationToken>) -> Result<Output, RunError> {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| RunError::Spawn(program.to_string(), e))?;
    let wait = child.wait_with_output();
    let never = CancellationToken::new();
    let cancel = cancel.unwrap_or(&never);
    // Dropping the future kills the child (kill_on_drop).
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(RunError::Cancelled),
        _ = tokio::time::sleep(timeout) => Err(RunError::Timeout(program.to_string())),
        out = wait => {
            let out = out.map_err(|e| RunError::Spawn(program.to_string(), e))?;
            Ok(Output { status: out.status.code(), stdout: out.stdout, stderr: out.stderr })
        }
    }
}

pub fn args<I, S>(items: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    items.into_iter().map(Into::into).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_tool_is_reported() {
        let err = run("/nonexistent/tool", &[], Duration::from_secs(1), None).await.unwrap_err();
        assert!(matches!(err, RunError::Spawn(..)));
    }

    #[tokio::test]
    async fn cancellation_kills_the_child() {
        let token = CancellationToken::new();
        token.cancel();
        // The current test binary is always present.
        let me = std::env::current_exe().unwrap().display().to_string();
        let err = run(&me, &args(["--list"]), Duration::from_secs(5), Some(&token)).await;
        assert!(matches!(err, Err(RunError::Cancelled)));
        let out = run(&me, &args(["--list"]), Duration::from_secs(30), None).await.unwrap();
        assert!(out.success());
    }
}
