//! `subprocess.run` with loki.py's deadline (`command_timeout`) and error texts.

use crate::{CheckError, py};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const COMMAND_TIMEOUT: f64 = 120.0;

/// A hook's overall deadline: `time.monotonic() + 20`, or none.
#[derive(Clone, Copy, Debug)]
pub struct Deadline(pub Option<Instant>);

impl Deadline {
    pub fn none() -> Deadline {
        Deadline(None)
    }

    pub fn after(seconds: f64) -> Deadline {
        Deadline(Some(Instant::now() + Duration::from_secs_f64(seconds)))
    }

    /// `command_timeout`: seconds left, at most 120.
    pub fn timeout(&self) -> Result<f64, CheckError> {
        let remaining = match self.0 {
            None => COMMAND_TIMEOUT,
            Some(at) => COMMAND_TIMEOUT.min(at.saturating_duration_since(Instant::now()).as_secs_f64()),
        };
        if remaining <= 0.0 {
            return Err(CheckError::Value("loki: hook deadline exceeded".into()));
        }
        Ok(remaining)
    }
}

pub struct Output {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Why a command did not run to completion: `OSError` or `TimeoutExpired`.
#[derive(Debug)]
pub enum RunError {
    Os(&'static str, String),
    Timeout(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Os(_, m) | RunError::Timeout(m) => f.write_str(m),
        }
    }
}

/// `str(list)` of a command, as Python prints it in `TimeoutExpired`.
fn command_repr(command: &[String]) -> String {
    format!(
        "[{}]",
        command.iter().map(|c| py::repr_str(c)).collect::<Vec<_>>().join(", ")
    )
}

/// Run `command` in `cwd`, feeding `input`, killed after `timeout` seconds.
pub fn run(
    command: &[String],
    cwd: &Path,
    input: Option<&[u8]>,
    env: &[(&str, &str)],
    timeout: f64,
) -> Result<Output, RunError> {
    let mut builder = Command::new(&command[0]);
    builder
        .args(&command[1..])
        .current_dir(cwd)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        builder.env(key, value);
    }
    let mut child = builder.spawn().map_err(|e| {
        let name = if e.kind() == std::io::ErrorKind::NotFound {
            Some(command[0].as_str())
        } else {
            None
        };
        // CPython names the cwd when chdir fails and the program when exec fails.
        let name = if !cwd.is_dir() {
            Some(cwd.to_str().unwrap_or(""))
        } else {
            name
        };
        RunError::Os(crate::os_kind(&e), py::os_error(&e, name))
    })?;
    let writer = match (child.stdin.take(), input) {
        (Some(mut stdin), Some(data)) => {
            let data = data.to_vec();
            Some(std::thread::spawn(move || {
                let _ = stdin.write_all(&data);
            }))
        }
        _ => None,
    };
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| RunError::Os("OSError", "no stdout".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| RunError::Os("OSError", "no stderr".into()))?;
    let out_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let err_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer);
        buffer
    });
    let started = Instant::now();
    let limit = Duration::from_secs_f64(timeout.max(0.0));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Timeout(format!(
                    "Command '{}' timed out after {} seconds",
                    command_repr(command),
                    py::float_repr(timeout)
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
            Err(e) => return Err(RunError::Os(crate::os_kind(&e), py::os_error(&e, None))),
        }
    };
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    let code = status.code().unwrap_or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        -status.signal().unwrap_or(0)
    });
    Ok(Output { code, stdout, stderr })
}

/// Text-mode output: `errors="replace"` decoding and universal newlines.
pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n").replace('\r', "\n")
}

/// `shutil.which(name)`.
pub fn which(name: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let executable =
        |path: &Path| std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if name.contains('/') {
        return executable(Path::new(name)).then(|| name.to_string());
    }
    let path = std::env::var("PATH").unwrap_or_default();
    path.split(':')
        .map(|dir| {
            if dir.is_empty() {
                Path::new(".").join(name)
            } else {
                Path::new(dir).join(name)
            }
        })
        .find(|candidate| executable(candidate))
        .map(|candidate| candidate.to_string_lossy().into_owned())
}

/// `Path.read_text(encoding="utf-8")`: strict UTF-8 with universal newlines.
pub fn read_text(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| py::os_error(&e, path.to_str()))?;
    let text = decode(&bytes)?;
    Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// `bytes.decode("utf-8")` with CPython's error text.
pub fn decode(bytes: &[u8]) -> Result<String, String> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text.to_string()),
        Err(error) => {
            let position = error.valid_up_to();
            let byte = bytes[position];
            Err(match error.error_len() {
                None if bytes.len() - position > 1 => format!(
                    "'utf-8' codec can't decode bytes in position {position}-{}: unexpected end of data",
                    bytes.len() - 1
                ),
                None => format!(
                    "'utf-8' codec can't decode byte 0x{byte:02x} in position {position}: unexpected end of data"
                ),
                Some(_) if (0x80..0xc2).contains(&byte) || byte > 0xf4 => {
                    format!("'utf-8' codec can't decode byte 0x{byte:02x} in position {position}: invalid start byte")
                }
                Some(_) => format!(
                    "'utf-8' codec can't decode byte 0x{byte:02x} in position {position}: invalid continuation byte"
                ),
            })
        }
    }
}
