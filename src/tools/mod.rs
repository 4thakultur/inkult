//! Integration with external command-line tools.
//!
//! Processes are always spawned directly (never through a shell) with each
//! argument passed separately, so untrusted input such as search queries or
//! URLs cannot inject commands. Every invocation is bounded in time and in
//! captured output size, and the child is killed if its future is dropped.

pub mod ffmpeg;
pub mod ytdlp;

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// Bytes of stderr retained for error reporting.
const STDERR_TAIL_BYTES: usize = 16 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{tool} is not installed or not executable ({program}): {source}")]
    NotFound {
        tool: &'static str,
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{tool} timed out after {secs}s")]
    Timeout { tool: &'static str, secs: u64 },
    #[error("{tool} produced more than {limit} bytes of output")]
    OutputTooLarge { tool: &'static str, limit: usize },
    #[error("{tool} failed ({status}): {message}")]
    Failed {
        tool: &'static str,
        status: String,
        /// Most relevant error lines from stderr.
        message: String,
    },
    #[error("I/O error while running {tool}: {source}")]
    Io {
        tool: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("{tool} returned unparseable output: {reason}")]
    InvalidOutput { tool: &'static str, reason: String },
    #[error("{tool} is busy with too many simultaneous streams; try again shortly")]
    Busy { tool: &'static str },
    #[error("operation cancelled")]
    Cancelled,
}

/// Availability of an external tool, reported in the UI.
#[derive(Debug, Clone, Serialize)]
pub struct ToolStatus {
    pub name: &'static str,
    pub program: String,
    pub version: Option<String>,
    pub error: Option<String>,
}

impl ToolStatus {
    pub fn available(&self) -> bool {
        self.version.is_some()
    }
}

pub(crate) struct ToolOutput {
    pub stdout: Vec<u8>,
}

pub(crate) fn spawn_error(tool: &'static str, program: &str, source: std::io::Error) -> ToolError {
    match source.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => {
            ToolError::NotFound {
                tool,
                program: program.to_owned(),
                source,
            }
        }
        _ => ToolError::Io { tool, source },
    }
}

/// Run a command to completion, capturing at most `max_stdout` bytes.
pub(crate) async fn run(
    tool: &'static str,
    mut cmd: Command,
    timeout: Duration,
    max_stdout: usize,
) -> Result<ToolOutput, ToolError> {
    let program = cmd.as_std().get_program().to_string_lossy().into_owned();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| spawn_error(tool, &program, e))?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(ToolError::Io {
            tool,
            source: std::io::Error::other("child pipes unavailable"),
        });
    };

    let work = async {
        let (stdout, stderr) = tokio::try_join!(
            read_capped(stdout, max_stdout, tool),
            read_tail(stderr, STDERR_TAIL_BYTES, tool)
        )?;
        let status = child
            .wait()
            .await
            .map_err(|source| ToolError::Io { tool, source })?;
        Ok::<_, ToolError>((stdout, stderr, status))
    };

    // On timeout or error the child is dropped and killed (`kill_on_drop`).
    let (stdout, stderr, status) =
        tokio::time::timeout(timeout, work)
            .await
            .map_err(|_| ToolError::Timeout {
                tool,
                secs: timeout.as_secs(),
            })??;

    if !status.success() {
        return Err(failure(tool, status, &stderr));
    }
    Ok(ToolOutput { stdout })
}

pub(crate) fn failure(tool: &'static str, status: ExitStatus, stderr: &str) -> ToolError {
    ToolError::Failed {
        tool,
        status: status.to_string(),
        message: relevant_error_lines(stderr),
    }
}

/// Prefer lines tagged `ERROR:` (yt-dlp, ffmpeg); otherwise the last line.
fn relevant_error_lines(stderr: &str) -> String {
    let errors: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("ERROR:") || l.starts_with("Error"))
        .collect();
    let message = if errors.is_empty() {
        stderr
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .unwrap_or("no error output")
            .to_owned()
    } else {
        errors.join("\n")
    };
    message.chars().take(2000).collect()
}

async fn read_capped<R: AsyncRead + Unpin>(
    reader: R,
    limit: usize,
    tool: &'static str,
) -> Result<Vec<u8>, ToolError> {
    let mut buf = Vec::new();
    let read_limit = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    reader
        .take(read_limit)
        .read_to_end(&mut buf)
        .await
        .map_err(|source| ToolError::Io { tool, source })?;
    if buf.len() > limit {
        return Err(ToolError::OutputTooLarge { tool, limit });
    }
    Ok(buf)
}

/// Drain a stream completely (so the child never blocks on a full pipe) and
/// return its last `keep` bytes as text.
pub(crate) async fn read_tail<R: AsyncRead + Unpin>(
    mut reader: R,
    keep: usize,
    tool: &'static str,
) -> Result<String, ToolError> {
    let mut tail: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = reader
            .read(&mut chunk)
            .await
            .map_err(|source| ToolError::Io { tool, source })?;
        if n == 0 {
            break;
        }
        tail.extend_from_slice(&chunk[..n]);
        if tail.len() > keep * 2 {
            tail.drain(..tail.len() - keep);
        }
    }
    if tail.len() > keep {
        tail.drain(..tail.len() - keep);
    }
    Ok(String::from_utf8_lossy(&tail).into_owned())
}

/// Aborts a task when dropped, e.g. when the work using it times out or is
/// cancelled.
pub struct AbortOnDrop<T>(pub tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_error_lines() {
        let stderr = "[youtube] abc: Downloading webpage\nWARNING: something\nERROR: [youtube] abc: Video unavailable\n";
        assert_eq!(
            relevant_error_lines(stderr),
            "ERROR: [youtube] abc: Video unavailable"
        );
        assert_eq!(relevant_error_lines("foo\nbar\n\n"), "bar");
    }

    #[tokio::test]
    async fn missing_program_is_reported_as_not_found() {
        let cmd = Command::new("/nonexistent/definitely-not-a-tool");
        let err = run("test-tool", cmd, Duration::from_secs(5), 1024)
            .await
            .err()
            .unwrap();
        assert!(matches!(err, ToolError::NotFound { .. }), "{err:?}");
    }
}
