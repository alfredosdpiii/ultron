//! Unix-domain socket transport (port of `transports/unix`): one listener per logical server at
//! `<server dir>/<serverId>.sock`, owner-only permissions, stale-socket recovery that never deletes a
//! live listener or a non-socket file, and cleanup that only removes the inode it bound.

use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use protocol::is_server_id;

use crate::connection::{Connection, Output, ServerConfig, ServerServices};

pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(5_000);
const SOCKET_MODE: u32 = 0o600;
const ACCEPT_POLL: Duration = Duration::from_millis(10);

/// Derive the local Unix socket path for one logical server identity.
pub fn unix_socket_path(server_id: &str, server_directory: &Path) -> Result<PathBuf, String> {
    if !is_server_id(server_id) {
        return Err("Unix serverId must be a canonical lowercase UUIDv4".into());
    }
    Ok(server_directory.join(format!("{server_id}.sock")))
}

fn other(message: String) -> io::Error {
    io::Error::other(message)
}

fn is_live(path: &Path) -> io::Result<bool> {
    match UnixStream::connect(path) {
        Ok(_) => Ok(true),
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::NotFound | ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

fn remove_stale_socket(path: &Path) -> io::Result<()> {
    let original = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !original.file_type().is_socket() {
        return Err(other(format!(
            "Refusing to remove non-socket Unix listener path: {}",
            path.display()
        )));
    }
    if is_live(path)? {
        return Err(other(format!("Unix listener is already running: {}", path.display())));
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

pub struct UnixServer {
    path: PathBuf,
    identity: (u64, u64),
    stopping: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
}

impl UnixServer {
    /// Bind `path` and serve each connection on its own thread with fresh services from `services`.
    pub fn start<S, F>(path: &Path, config: ServerConfig, handshake_timeout: Duration, services: F) -> io::Result<Self>
    where
        S: ServerServices + Send + 'static,
        F: Fn() -> S + Send + 'static,
    {
        if let Some(parent) = path.parent() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        }
        remove_stale_socket(path)?;
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE))?;
        let meta = std::fs::symlink_metadata(path)?;
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        // ponytail: the accept loop polls a stop flag every 10 ms; std cannot interrupt a blocking accept.
        let accept_thread = std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if stream.set_nonblocking(false).is_err() {
                            continue;
                        }
                        let connection = Connection::new(config.clone(), services());
                        std::thread::spawn(move || serve(stream, connection, handshake_timeout));
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(ACCEPT_POLL),
                    Err(_) => std::thread::sleep(ACCEPT_POLL),
                }
            }
        });
        Ok(Self {
            path: path.to_owned(),
            identity: (meta.dev(), meta.ino()),
            stopping,
            accept_thread: Some(accept_thread),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stop accepting and remove the socket, unless the path now holds a different inode.
    pub fn close(mut self) -> io::Result<()> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> io::Result<()> {
        let Some(thread) = self.accept_thread.take() else {
            return Ok(());
        };
        self.stopping.store(true, Ordering::SeqCst);
        let _ = thread.join();
        match std::fs::symlink_metadata(&self.path) {
            Ok(meta) if meta.file_type().is_socket() && (meta.dev(), meta.ino()) == self.identity => {
                std::fs::remove_file(&self.path)
            }
            _ => Ok(()),
        }
    }
}

impl Drop for UnixServer {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn serve<S: ServerServices>(mut stream: UnixStream, mut connection: Connection<S>, handshake_timeout: Duration) {
    let _ = stream.set_read_timeout(Some(handshake_timeout));
    let mut buffer = vec![0u8; 64 * 1024];
    while !connection.is_closed() {
        let outputs = match stream.read(&mut buffer) {
            Ok(0) => {
                connection.transport_closed();
                break;
            }
            Ok(n) => connection.receive(&buffer[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => connection.handshake_timeout(),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => {
                connection.transport_closed();
                break;
            }
        };
        if connection.is_ready() {
            let _ = stream.set_read_timeout(None);
        }
        for output in outputs {
            match output {
                Output::Send(frame) => {
                    if stream.write_all(&frame).is_err() {
                        connection.transport_closed();
                    }
                }
                Output::Close(frame) => {
                    if let Some(frame) = frame {
                        let _ = stream.write_all(&frame);
                    }
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
        }
    }
}
