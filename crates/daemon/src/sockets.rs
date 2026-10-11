//! Unix socket helpers shared by the coordinator and server generations.

use std::io::{self, ErrorKind};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

/// Connect, treating a missing or refusing socket as absent (`tryConnect`).
pub fn try_connect(path: &Path) -> io::Result<Option<UnixStream>> {
    match UnixStream::connect(path) {
        Ok(stream) => Ok(Some(stream)),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn is_live(path: &Path) -> bool {
    matches!(try_connect(path), Ok(Some(_)))
}

/// `removeStaleSocket`: a dead socket is removed; a live one or a non-socket file is an error.
pub fn remove_stale_socket(path: &Path, what: &str) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
        Ok(meta) if !meta.file_type().is_socket() => {
            return Err(io::Error::other(format!(
                "{what} path is not a socket: {}",
                path.display()
            )));
        }
        Ok(_) => {}
    }
    if try_connect(path)?.is_some() {
        return Err(io::Error::other(format!(
            "{what} socket is already active: {}",
            path.display()
        )));
    }
    cleanup_socket(path)
}

pub fn cleanup_socket(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Bind with owner-only permissions.
pub fn listen(path: &Path) -> io::Result<UnixListener> {
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}
