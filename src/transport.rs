//! Persistent JSON-Lines agent subprocess transport — port of
//! `challenge/run_challenge.py::JsonLineAgentProcess` (threaded path) and the
//! process-isolation parts of `local_runner.py` (`load_dotenv`,
//! `build_agent_env`, process-group kill).
//!
//! Threading model mirrors Python's `SAC_TRANSPORT=threads` path: a pump
//! thread streams stdout chunks into a channel, and each write runs on a
//! worker thread joined with the deadline — so the global wall clock holds
//! even if the agent wedges mid-message. The child gets its own process group
//! (`setpgid`) so kills are group-wide (SIGTERM, then SIGKILL after 2s;
//! immediate SIGKILL when forced).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::contracts::PARTICIPANT_PROTOCOL_VERSION;

/// Raised by the transport at the global cutoff; the workflow maps it to
/// `global_wallclock_expired` with `ignored_in_flight_response = true`.
#[derive(Debug)]
pub struct GlobalDeadlineExpired;

impl std::fmt::Display for GlobalDeadlineExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "global wallclock deadline expired")
    }
}

impl std::error::Error for GlobalDeadlineExpired {}

pub fn is_global_deadline_expired(error: &anyhow::Error) -> bool {
    error.downcast_ref::<GlobalDeadlineExpired>().is_some()
}

pub const PROTECTED_KEYS: [&str; 6] =
    ["PATH", "HOME", "TMPDIR", "LD_PRELOAD", "PYTHONPATH", "PYTHONSTARTUP"];

/// `^[A-Z][A-Z0-9_]{0,63}$`
fn safe_env_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_uppercase()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
}

/// Parse KEY=VALUE lines (no interpolation), the same reader the platform uses.
pub fn load_dotenv(path: &Path) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return env;
    };
    for raw in text.split('\n') {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let (key, value) = line.split_once('=').unwrap();
        let key = key.trim();
        let mut value = value.trim();
        let bytes = value.as_bytes();
        if bytes.len() >= 2 && bytes[0] == bytes[bytes.len() - 1] && (bytes[0] == b'\'' || bytes[0] == b'"') {
            value = &value[1..value.len() - 1];
        }
        if safe_env_key(key) {
            env.insert(key.to_string(), value.to_string());
        }
    }
    env
}

/// Scrubbed child environment: PATH only, HOME/TMPDIR pointed at the scratch
/// directory, plus the contract variables and the filtered `.env` pairs.
pub fn build_agent_env(
    agent_dir: &Path,
    scratch: &Path,
    wallclock: f64,
    scenario_slug: &str,
    protocol_version: &str,
) -> (Vec<(String, String)>, Vec<String>) {
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string());
    let mut env = vec![
        ("PATH".to_string(), path),
        ("HOME".to_string(), scratch.to_string_lossy().into_owned()),
        ("TMPDIR".to_string(), scratch.to_string_lossy().into_owned()),
        ("LANG".to_string(), "C.UTF-8".to_string()),
        ("LC_ALL".to_string(), "C.UTF-8".to_string()),
        ("PYTHONUNBUFFERED".to_string(), "1".to_string()),
        ("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string()),
        ("PYTHONIOENCODING".to_string(), "utf-8".to_string()),
        ("PARTICIPANT_PROTOCOL".to_string(), protocol_version.to_string()),
        ("SAC_SCENARIO".to_string(), scenario_slug.to_string()),
        ("SAC_WALLCLOCK_SECONDS".to_string(), format!("{}", wallclock as i64)),
        ("SAC_LOCAL_RUNNER".to_string(), "1".to_string()),
    ];
    let dotenv = load_dotenv(&agent_dir.join(".env"));
    let keys: Vec<String> = dotenv.keys().cloned().collect();
    for (key, value) in dotenv {
        if !PROTECTED_KEYS.contains(&key.as_str()) {
            env.push((key, value));
        }
    }
    (env, keys)
}

pub struct AgentProcess {
    command: String,
    agent_dir: PathBuf,
    env: Vec<(String, String)>,
    stderr_log: Option<File>,
    initialization_timeout_seconds: f64,
    protocol_version: String,
    child: Option<Child>,
    stdin: Option<Arc<Mutex<ChildStdin>>>,
    chunks_tx: Sender<Vec<u8>>,
    chunks_rx: Receiver<Vec<u8>>,
    stdout_buffer: Vec<u8>,
}

impl AgentProcess {
    /// `command` is run via `sh -c` with `cwd = agent_dir` and the scrubbed `env`.
    pub fn new(
        command: &str,
        agent_dir: PathBuf,
        env: Vec<(String, String)>,
        stderr_log: Option<File>,
        initialization_timeout_seconds: f64,
        protocol_version: &str,
    ) -> Result<Self> {
        if command.trim().is_empty() {
            bail!("agent command cannot be empty");
        }
        if initialization_timeout_seconds <= 0.0 {
            bail!("initialization timeout must be positive");
        }
        let (chunks_tx, chunks_rx) = mpsc::channel();
        Ok(Self {
            command: command.to_string(),
            agent_dir,
            env,
            stderr_log,
            initialization_timeout_seconds,
            protocol_version: protocol_version.to_string(),
            child: None,
            stdin: None,
            chunks_tx,
            chunks_rx,
            stdout_buffer: Vec::new(),
        })
    }

    fn start(&mut self) -> Result<()> {
        if self.child.is_some() {
            return Ok(());
        }
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(&self.command)
            .current_dir(&self.agent_dir)
            .env_clear()
            .envs(self.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        command.stderr(match &self.stderr_log {
            Some(file) => Stdio::from(file.try_clone()?),
            None => Stdio::null(),
        });
        // Own process group so close() can kill the whole tree (sh + children).
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("starting agent command {:?}", self.command))?;
        let stdout: ChildStdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        self.stdin = Some(Arc::new(Mutex::new(stdin)));
        let tx = self.chunks_tx.clone();
        thread::spawn(move || {
            let mut stdout = stdout;
            let mut buffer = [0u8; 65536];
            loop {
                let chunk = match stdout.read(&mut buffer) {
                    Ok(0) | Err(_) => Vec::new(),
                    Ok(read) => buffer[..read].to_vec(),
                };
                let _ = tx.send(chunk.clone());
                if chunk.is_empty() {
                    return;
                }
            }
        });
        self.child = Some(child);
        Ok(())
    }

    fn write_message(&mut self, message: &Value, deadline: Instant) -> Result<()> {
        self.start()?;
        let mut data = serde_json::to_string(message)?.into_bytes();
        data.push(b'\n');
        let stdin = self.stdin.clone().unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let result = stdin
                .lock()
                .map_err(|error| anyhow!("stdin lock poisoned: {error}"))
                .and_then(|mut guard| {
                    guard.write_all(&data).and_then(|_| guard.flush()).map_err(Into::into)
                });
            let _ = done_tx.send(result);
        });
        let remaining = deadline.saturating_duration_since(Instant::now());
        match done_rx.recv_timeout(remaining) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(anyhow!("agent exited before reading the next message ({error})")),
            Err(_) => {
                let _ = self.close(true);
                Err(GlobalDeadlineExpired.into())
            }
        }
    }

    fn read_line(&mut self, deadline: Instant) -> Result<Value> {
        loop {
            if let Some(position) = self.stdout_buffer.iter().position(|byte| *byte == b'\n') {
                let mut line: Vec<u8> = self.stdout_buffer.drain(..=position).collect();
                line.pop();
                let payload: Value = serde_json::from_slice(&line)
                    .context("agent response is not valid JSON")?;
                if !payload.is_object() {
                    bail!("agent response must be a JSON object");
                }
                return Ok(payload);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.close(true);
                return Err(GlobalDeadlineExpired.into());
            }
            match self.chunks_rx.recv_timeout(remaining) {
                Ok(chunk) if chunk.is_empty() => {
                    let code = self
                        .child
                        .as_mut()
                        .and_then(|child| child.try_wait().ok().flatten());
                    bail!("agent exited before responding (code={code:?})");
                }
                Ok(chunk) => self.stdout_buffer.extend_from_slice(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let _ = self.close(true);
                    return Err(GlobalDeadlineExpired.into());
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("agent exited before responding (code=None)");
                }
            }
        }
    }

    /// Send the one-time public bootstrap message before the competition clock.
    pub fn publish_initial(&mut self, publication: &Value) -> Result<()> {
        let deadline =
            Instant::now() + Duration::from_secs_f64(self.initialization_timeout_seconds);
        self.write_message(
            &serde_json::json!({
                "protocol_version": self.protocol_version,
                "message_type": "initialize",
                "payload": publication,
            }),
            deadline,
        )
    }

    /// Send one decision snapshot and read one response, bounded by the global
    /// deadline (Python `__call__`).
    pub fn call(&mut self, snapshot: &Value, deadline: Instant) -> Result<Value> {
        self.write_message(
            &serde_json::json!({
                "protocol_version": self.protocol_version,
                "message_type": "decision_request",
                "decision_sequence": snapshot["decision_sequence"],
                "payload": snapshot,
            }),
            deadline,
        )?;
        self.read_line(deadline)
    }

    /// Terminate (SIGTERM, SIGKILL after 2s) or immediately kill (SIGKILL) the
    /// whole process group, then reap.
    pub fn close(&mut self, force: bool) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        if child.try_wait()?.is_none() {
            #[cfg(unix)]
            unsafe {
                let pid = child.id() as i32;
                libc::kill(-pid, if force { libc::SIGKILL } else { libc::SIGTERM });
            }
            let started = Instant::now();
            loop {
                if child.try_wait()?.is_some() {
                    break;
                }
                if started.elapsed() >= Duration::from_secs(2) {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        let _ = child.wait();
        self.stdin = None;
        Ok(())
    }
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        let _ = self.close(true);
    }
}

/// Protocol the participant speaks: v2 when the scenario's anomaly mechanics
/// are enabled, v1 otherwise.
pub fn protocol_version_for(mechanics: bool) -> &'static str {
    if mechanics {
        PARTICIPANT_PROTOCOL_VERSION
    } else {
        crate::contracts::LEGACY_PARTICIPANT_PROTOCOL_VERSION
    }
}
