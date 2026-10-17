//! Crash-safe writes for Inkult's own state files (library index, users).

use std::io::{self, Write};
use std::path::Path;

/// Who may read the written file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Default permissions (umask).
    Normal,
    /// Owner-only (0600 on Unix), for files holding credentials.
    Private,
}

/// Replace `path` atomically: write a temp file next to it, fsync, rename.
/// A crash leaves either the old or the new file, never a truncated one.
/// Blocking; call from `spawn_blocking` in async code.
pub fn write_atomic(path: &Path, bytes: &[u8], visibility: Visibility) -> io::Result<()> {
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if visibility == Visibility::Private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    #[cfg(unix)]
    if visibility == Visibility::Private {
        // `mode` only applies on creation; tighten a pre-existing temp file.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_file_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        write_atomic(&path, b"one", Visibility::Normal).unwrap();
        write_atomic(&path, b"two", Visibility::Private).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
