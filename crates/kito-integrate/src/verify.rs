//! The verifier: runs the configured commands on a merged result.

use crate::result::VerificationReport;
use std::ffi::OsString;
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

/// How the commands are started: as they are, or through a program that
/// confines them (the daemon's sandbox).
#[derive(Clone, Debug, Default)]
pub struct Confinement {
    /// What is put before `sh -c <command>`: a program and its arguments.
    /// Empty: the commands are not confined.
    pub wrapper: Vec<OsString>,
    /// What is said after the output of a command that failed confined:
    /// the failure may be the confinement's doing.
    pub hint: Option<String>,
}

/// Runs `commands` in order with `sh -c`, in `dir`, and stops at the first
/// one that fails. A command that runs longer than `timeout` is killed, with
/// everything it started, and counts as failed.
///
/// The commands see `KITO_VERIFY=1`, and nothing on their standard input.
pub async fn verify(
    dir: &Path,
    commands: &[String],
    timeout: Duration,
    confinement: &Confinement,
) -> VerificationReport {
    verify_with_shell(dir, commands, timeout, confinement).await
}

/// The shell the checks are written for: `sh`; on Windows `sh` where one is
/// on the `PATH` (Git for Windows has one), else `cmd` (ADR 0002).
pub fn shell_program() -> (&'static str, &'static str) {
    if cfg!(windows) {
        let found = std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("sh.exe").is_file()));
        if !found {
            return ("cmd", "/C");
        }
    }
    ("sh", "-c")
}

async fn verify_with_shell(
    dir: &Path,
    commands: &[String],
    timeout: Duration,
    confinement: &Confinement,
) -> VerificationReport {
    let started = Instant::now();
    let mut ran = Vec::new();
    let mut failure: Option<(String, String)> = None;
    let confined = !confinement.wrapper.is_empty();
    for command in commands {
        ran.push(command.clone());
        let (program, flag) = shell_program();
        let mut shell = match confinement.wrapper.split_first() {
            Some((wrapper, args)) => {
                let mut wrapped = Command::new(wrapper);
                wrapped.args(args).arg(program);
                wrapped
            }
            None => Command::new(program),
        };
        shell
            .arg(flag)
            .arg(command)
            .current_dir(dir)
            .env("KITO_VERIFY", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // A group of its own: on a timeout the whole of it is killed, not
        // only the shell.
        #[cfg(unix)]
        shell.process_group(0);
        let spawned = shell.spawn();
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
                let hint = match &confinement.hint {
                    Some(hint) => format!("\n({hint})"),
                    None => String::new(),
                };
                failure =
                    Some((command.clone(), format!("{}\n({how}){hint}", tail(output)).trim().to_owned()));
                break;
            }
            Ok((Err(e), _)) => {
                failure = Some((command.clone(), format!("could not be waited for: {e}")));
                break;
            }
            Err(_) => {
                #[cfg(unix)]
                if let Some(pid) = pid.and_then(|pid| rustix::process::Pid::from_raw(pid as i32)) {
                    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                }
                // On Windows, the command's tree (ADR 0002).
                #[cfg(windows)]
                if let Some(pid) = pid {
                    let _ = std::process::Command::new("taskkill")
                        .args(["/T", "/F", "/PID", &pid.to_string()])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
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
        confined,
        batch: Vec::new(),
        waits: None,
    }
}

/// What a program that ran to its end left behind.
#[derive(Clone, Debug)]
pub struct Ran {
    pub success: bool,
    /// How it ended: `exit code 2`, `killed by a signal`.
    pub how: String,
    pub stdout: Vec<u8>,
    /// The end of what it printed as errors.
    pub errors: String,
}

/// Runs one program in `dir`, with `input` on its standard input, confined
/// like the commands of [`verify`]. `Err`: it could not be started, or
/// was stopped after `timeout`.
pub async fn run_program(
    dir: &Path,
    program: &std::ffi::OsStr,
    args: &[OsString],
    input: &[u8],
    envs: &[(OsString, OsString)],
    timeout: Duration,
    confinement: &Confinement,
) -> Result<Ran, String> {
    use tokio::io::AsyncWriteExt;
    let mut command = match confinement.wrapper.split_first() {
        Some((wrapper, wrapped)) => {
            let mut command = Command::new(wrapper);
            command.args(wrapped).arg(program);
            command
        }
        None => Command::new(program),
    };
    command
        .args(args)
        .current_dir(dir)
        .env("KITO_VERIFY", "1")
        .envs(envs.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|e| format!("could not be started: {e}"))?;
    let (mut stdin, mut stdout, mut stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let pid = child.id();
    let run = async {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let write = async {
            if let Some(mut pipe) = stdin.take() {
                // One that does not read what it is told is its own business.
                let _ = pipe.write_all(input).await;
                let _ = pipe.shutdown().await;
            }
        };
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
        let ((), (), (), status) = tokio::join!(write, read_out, read_err, child.wait());
        (status, out, err)
    };
    match tokio::time::timeout(timeout, run).await {
        Ok((Ok(status), stdout, errors)) => Ok(Ran {
            success: status.success(),
            how: match status.code() {
                Some(code) => format!("exit code {code}"),
                None => "killed by a signal".to_owned(),
            },
            stdout,
            errors: tail(errors),
        }),
        Ok((Err(e), ..)) => Err(format!("could not be waited for: {e}")),
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = pid.and_then(|pid| rustix::process::Pid::from_raw(pid as i32)) {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
            #[cfg(windows)]
            if let Some(pid) = pid {
                let _ = std::process::Command::new("taskkill")
                    .args(["/T", "/F", "/PID", &pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = child.kill().await;
            Err(format!("still running after {} s: stopped", timeout.as_secs()))
        }
    }
}
