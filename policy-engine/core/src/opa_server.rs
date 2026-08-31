//! Long-lived `opa run --server` children for the Rego dispatcher (`ACS_OPA_SERVER=1`).
//!
//! One server per distinct policy source set (the file set an exec-mode evaluation
//! would pass as `--data`), spawned lazily on first use and killed when the owning
//! dispatcher drops. Evaluations become Data API requests over a Unix domain socket
//! in a private scratch directory. Transport-level failures surface as
//! [`QueryFailure::Unavailable`] so the caller can fall back to the exec path —
//! evaluation is pure, so re-running it is safe; only an evaluation timeout or an
//! OPA-reported error is a hard failure, matching exec-mode semantics.

use crate::opa::{create_private_dir, opa_command_path_arg, private_dir_token, truncate};
use crate::JsonValue;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

const SPAWN_DEADLINE: Duration = Duration::from_secs(5);
const SPAWN_RETRY_COOLDOWN: Duration = Duration::from_secs(30);
const READY_POLL: Duration = Duration::from_millis(5);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) enum QueryFailure {
    /// The server could not be reached or died mid-request; the evaluation did not
    /// complete and the exec path should decide instead.
    Unavailable,
    /// The evaluation ran past the configured timeout — fail closed, as exec mode does.
    Timeout(u128),
    /// OPA answered with an error or an undefined result.
    Eval(String),
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ServerCache {
    entries: Arc<Mutex<HashMap<Vec<PathBuf>, CacheEntry>>>,
}

#[derive(Debug, Clone)]
enum CacheEntry {
    Ready(Arc<OpaServer>),
    FailedAt(Instant),
}

impl ServerCache {
    /// The ready server for this source set, spawning one if needed. `None` means
    /// server evaluation is not available right now (spawn failed, or failed recently
    /// enough that retrying would stall every evaluation behind the spawn deadline).
    pub(crate) fn lease(&self, executable: &Path, sources: &[PathBuf]) -> Option<Arc<OpaServer>> {
        // Spawning under the lock is deliberate: a concurrent stampede must produce
        // one server, and only first use pays the wait.
        let mut entries = lock(&self.entries);
        match entries.get(sources) {
            Some(CacheEntry::Ready(server)) => return Some(server.clone()),
            Some(CacheEntry::FailedAt(at)) if at.elapsed() < SPAWN_RETRY_COOLDOWN => return None,
            _ => {}
        }
        match OpaServer::spawn(executable, sources) {
            Ok(server) => {
                let server = Arc::new(server);
                entries.insert(sources.to_vec(), CacheEntry::Ready(server.clone()));
                Some(server)
            }
            Err(_) => {
                entries.insert(sources.to_vec(), CacheEntry::FailedAt(Instant::now()));
                None
            }
        }
    }

    /// Forget a server that stopped answering so the next evaluation respawns one.
    /// Only the exact server the caller failed against is evicted — a concurrent
    /// evaluation may already have replaced it.
    pub(crate) fn evict(&self, sources: &[PathBuf], server: &Arc<OpaServer>) {
        let mut entries = lock(&self.entries);
        if let Some(CacheEntry::Ready(current)) = entries.get(sources) {
            if Arc::ptr_eq(current, server) {
                entries.remove(sources);
            }
        }
    }

    pub(crate) fn server_pids(&self) -> Vec<u32> {
        lock(&self.entries)
            .values()
            .filter_map(|entry| match entry {
                CacheEntry::Ready(server) => Some(server.pid),
                CacheEntry::FailedAt(_) => None,
            })
            .collect()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug)]
pub(crate) struct OpaServer {
    child: Mutex<Child>,
    pid: u32,
    socket: PathBuf,
    dir: PathBuf,
}

impl OpaServer {
    fn spawn(executable: &Path, sources: &[PathBuf]) -> io::Result<OpaServer> {
        let dir = std::env::temp_dir().join(format!("acs-opa-server-{}", private_dir_token()));
        create_private_dir(&dir)?;
        let socket = dir.join("opa.sock");
        let mut command = Command::new(executable);
        command
            .arg("run")
            .arg("--server")
            .arg("--addr")
            .arg(format!("unix://{}", socket.display()))
            .arg("--unix-socket-perm")
            .arg("600")
            .arg("--log-level")
            .arg("error")
            .arg("--disable-telemetry")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for source in sources {
            command.arg(opa_command_path_arg(source));
        }
        let child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(err);
            }
        };
        let pid = child.id();
        let server = OpaServer {
            child: Mutex::new(child),
            pid,
            socket,
            dir,
        };
        // On failure the server drops here, which kills the child and removes the dir.
        server.await_ready()?;
        Ok(server)
    }

    fn await_ready(&self) -> io::Result<()> {
        let deadline = Instant::now() + SPAWN_DEADLINE;
        loop {
            if let Some(status) = lock(&self.child).try_wait()? {
                return Err(io::Error::other(format!(
                    "opa server exited during startup with {status}"
                )));
            }
            if self.socket.exists() {
                if let Ok((200, _)) = self.request("GET", "/health", None, HEALTH_TIMEOUT) {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "opa server did not become healthy before the spawn deadline",
                ));
            }
            thread::sleep(READY_POLL);
        }
    }

    pub(crate) fn query(
        &self,
        data_path: &str,
        input: &str,
        timeout: Duration,
    ) -> Result<JsonValue, QueryFailure> {
        let body = format!("{{\"input\":{input}}}");
        let (status, response) = self
            .request(
                "POST",
                &format!("/v1/data/{data_path}"),
                Some(&body),
                timeout,
            )
            .map_err(|err| {
                if matches!(
                    err.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) {
                    QueryFailure::Timeout(timeout.as_millis())
                } else {
                    QueryFailure::Unavailable
                }
            })?;
        let parsed: JsonValue = serde_json::from_slice(&response)
            .map_err(|err| QueryFailure::Eval(format!("failed to parse OPA JSON output: {err}")))?;
        if status != 200 {
            return Err(QueryFailure::Eval(format!(
                "OPA returned errors: {}",
                opa_error_detail(&parsed, status)
            )));
        }
        match parsed.get("result") {
            Some(value) => Ok(value.clone()),
            None => Err(QueryFailure::Eval(
                "OPA query returned no result".to_string(),
            )),
        }
    }

    // HTTP/1.0 with Connection: close keeps the reply un-chunked and EOF-delimited,
    // so a header parse plus read-to-end is a complete client.
    fn request(
        &self,
        method: &str,
        target: &str,
        body: Option<&str>,
        timeout: Duration,
    ) -> io::Result<(u16, Vec<u8>)> {
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let mut request =
            format!("{method} {target} HTTP/1.0\r\nHost: opa\r\nConnection: close\r\n");
        if let Some(body) = body {
            request.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ));
        }
        request.push_str("\r\n");
        if let Some(body) = body {
            request.push_str(body);
        }
        stream.write_all(request.as_bytes())?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response)?;
        split_response(&response)
    }
}

impl Drop for OpaServer {
    fn drop(&mut self) {
        let mut child = lock(&self.child);
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn split_response(response: &[u8]) -> io::Result<(u16, Vec<u8>)> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| io::Error::other("opa server reply carried no header terminator"))?;
    let head = String::from_utf8_lossy(&response[..header_end]);
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| io::Error::other("opa server reply carried no status code"))?;
    Ok((status, response[header_end + 4..].to_vec()))
}

fn opa_error_detail(body: &JsonValue, status: u16) -> String {
    let code = body.get("code").and_then(JsonValue::as_str);
    let message = body.get("message").and_then(JsonValue::as_str);
    match (code, message) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (None, Some(message)) => message.to_string(),
        _ => truncate(&format!("HTTP {status}: {body}")),
    }
}

#[cfg(test)]
mod tests {
    use super::split_response;

    #[test]
    fn split_response_parses_status_and_body() {
        let raw = b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{\"result\":true}";
        let (status, body) = split_response(raw).expect("well-formed reply");
        assert_eq!(status, 200);
        assert_eq!(body, b"{\"result\":true}");
    }

    #[test]
    fn split_response_refuses_a_reply_without_headers() {
        assert!(split_response(b"garbage").is_err());
    }

    #[test]
    fn split_response_refuses_a_reply_without_a_status_code() {
        assert!(split_response(b"HTTP/1.0\r\n\r\n{}").is_err());
    }
}
