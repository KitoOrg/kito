//! The verifier: runs the configured commands on a merged result.

use crate::result::VerificationReport;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// How much of what a failed command printed is kept.
const OUTPUT_TAIL: usize = 4000;

fn tail(mut bytes: Vec<u8>) -> String {
    if bytes.len() > OUTPUT_TAIL {
        bytes.drain(..bytes.len() - OUTPUT_TAIL);
    }
    String::from_utf8_lossy(&bytes).trim_end().to_owned()
}

/// Runs `commands` in order with `sh -c`, in `dir`, and stops at the first
/// one that fails. A command that runs longer than `timeout` is killed, with
/// everything it started, and counts as failed.
///
/// The commands see `KITO_VERIFY=1`, and nothing on their standard input.
pub async fn verify(dir: &Path, commands: &[String], timeout: Duration) -> VerificationReport {
    let started = Instant::now();
    let mut ran = Vec::new();
    let mut failure: Option<(String, String)> = None;
    for command in commands {
        ran.push(command.clone());
        let spawned = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(dir)
            .env("KITO_VERIFY", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A group of its own: on a timeout the whole of it is killed,
            // not only the shell.
            .process_group(0)
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => {
                failure = Some((command.clone(), format!("could not be started: {e}")));
                break;
            }
        };
        let (mut stdout, mut stderr) = (child.stdout.take(), child.stderr.take());
        let pid = child.id();
        let run = async {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let read_out = async {
                if let Some(pipe) = stdout.as_mut() {
                    let _ = pipe.read_to_end(&mut out).await;
                }
            };
            let read_err = async {
                if let Some(pipe) = stderr.as_mut() {
                    let _ = pipe.read_to_end(&mut err).await;
                }
            };
            let ((), (), status) = tokio::join!(read_out, read_err, child.wait());
            out.extend_from_slice(&err);
            (status, out)
        };
        let outcome = tokio::time::timeout(timeout, run).await;
        match outcome {
            Ok((Ok(status), _)) if status.success() => {}
            Ok((Ok(status), output)) => {
                let how = match status.code() {
                    Some(code) => format!("exit code {code}"),
                    None => "killed by a signal".to_owned(),
                };
                failure = Some((command.clone(), format!("{}\n({how})", tail(output)).trim().to_owned()));
                break;
            }
            Ok((Err(e), _)) => {
                failure = Some((command.clone(), format!("could not be waited for: {e}")));
                break;
            }
            Err(_) => {
                if let Some(pid) = pid.and_then(|pid| rustix::process::Pid::from_raw(pid as i32)) {
                    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                }
                let _ = child.kill().await;
                failure =
                    Some((command.clone(), format!("still running after {} s: stopped", timeout.as_secs())));
                break;
            }
        }
    }
    VerificationReport {
        commands: ran,
        passed: failure.is_none(),
        duration_ms: started.elapsed().as_millis() as u64,
        failed_command: failure.as_ref().map(|(command, _)| command.clone()),
        output: failure.map(|(_, output)| output),
    }
}
