//! Moving finished files into the library without ever overwriting.
//!
//! Plain `rename` silently replaces an existing destination on Unix, so the
//! final step uses `hard_link`, which atomically fails if the destination
//! exists. The file is first moved into `<library>/.inkult-tmp/` so that the
//! link happens within one filesystem, even when the staging directory lives
//! on another disk.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const LIBRARY_TMP_DIR: &str = ".inkult-tmp";
const MAX_COLLISION_SUFFIX: u32 = 100;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Move `src` to `root/rel`, or to `root/<rel stem> (N).<ext>` if taken.
/// Returns the library-relative path actually used.
pub fn place_file(src: &Path, root: &Path, rel: &Path) -> io::Result<PathBuf> {
    if rel.file_name().is_none() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing non-normalized library path {}", rel.display()),
        ));
    }
    if let Some(parent) = rel.parent() {
        fs::create_dir_all(root.join(parent))?;
    }

    let tmp = move_into_library_tmp(src, root)?;
    let result = link_without_clobber(&tmp, root, rel);
    // On success `tmp` is either already renamed away or a second link to
    // the placed file; on failure it is a re-downloadable copy.
    let _ = fs::remove_file(&tmp);
    result
}

fn move_into_library_tmp(src: &Path, root: &Path) -> io::Result<PathBuf> {
    let tmp_dir = root.join(LIBRARY_TMP_DIR);
    fs::create_dir_all(&tmp_dir)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let unique = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = tmp_dir.join(format!("{}-{nanos}-{unique}.part", std::process::id()));

    match fs::rename(src, &tmp) {
        Ok(()) => Ok(tmp),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            let copied = (|| {
                fs::copy(src, &tmp)?;
                fs::File::open(&tmp)?.sync_all()
            })();
            if let Err(e) = copied {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
            fs::remove_file(src)?;
            Ok(tmp)
        }
        Err(e) => Err(e),
    }
}

fn link_without_clobber(tmp: &Path, root: &Path, rel: &Path) -> io::Result<PathBuf> {
    for n in 1..=MAX_COLLISION_SUFFIX {
        let candidate = if n == 1 {
            rel.to_path_buf()
        } else {
            with_suffix(rel, n)
        };
        let dest = root.join(&candidate);
        match fs::hard_link(tmp, &dest) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                // Filesystems without hard links (some FUSE/SMB mounts): fall
                // back to check-then-rename. This has a small race window, but
                // Inkult is the only writer of its own staging files.
                if dest.symlink_metadata().is_ok() {
                    continue;
                }
                fs::rename(tmp, &dest)?;
                return Ok(candidate);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("too many files named like {}", rel.display()),
    ))
}

fn with_suffix(rel: &Path, n: u32) -> PathBuf {
    let stem = rel.file_stem().and_then(|s| s.to_str()).unwrap_or("track");
    let name = match rel.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem} ({n}).{ext}"),
        None => format!("{stem} ({n})"),
    };
    rel.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_file_and_creates_directories() {
        let root = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let src = staging.path().join("media.opus");
        fs::write(&src, b"audio").unwrap();

        let rel = place_file(&src, root.path(), Path::new("Artist/Album/01 - T.opus")).unwrap();
        assert_eq!(rel, PathBuf::from("Artist/Album/01 - T.opus"));
        assert_eq!(fs::read(root.path().join(&rel)).unwrap(), b"audio");
        assert!(!src.exists());
        assert_eq!(
            fs::read_dir(root.path().join(LIBRARY_TMP_DIR))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn never_overwrites_existing_files() {
        let root = tempfile::tempdir().unwrap();
        let existing = root.path().join("A/Singles/T.opus");
        fs::create_dir_all(existing.parent().unwrap()).unwrap();
        fs::write(&existing, b"user's file").unwrap();

        let src = root.path().join("new.opus");
        fs::write(&src, b"new").unwrap();
        let rel = place_file(&src, root.path(), Path::new("A/Singles/T.opus")).unwrap();

        assert_eq!(rel, PathBuf::from("A/Singles/T (2).opus"));
        assert_eq!(fs::read(&existing).unwrap(), b"user's file");
        assert_eq!(fs::read(root.path().join(rel)).unwrap(), b"new");
    }

    #[test]
    fn rejects_escaping_paths() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("x");
        fs::write(&src, b"x").unwrap();
        for bad in ["../x.opus", "/abs/x.opus", "a/../../x.opus", ""] {
            assert!(
                place_file(&src, root.path(), Path::new(bad)).is_err(),
                "{bad}"
            );
        }
        assert!(src.exists(), "source must survive rejected placement");
    }
}
