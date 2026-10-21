//! Tufan's own copy list, filled by the file manager's "Tufan Copy" and
//! "Tufan Cut" actions and consumed by "Paste with Tufan". The system
//! clipboard is not used: Wayland only lets the focused app read it.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use tufan_core::journal;
use tufan_core::JobKind;

fn path() -> PathBuf {
    journal::default_path().with_file_name("copy-list")
}

/// Replaces the list. Relative paths are taken relative to `cwd`.
pub fn store(kind: JobKind, paths: &[PathBuf], cwd: &Path) -> io::Result<()> {
    let mut data = match kind {
        JobKind::Copy => b"copy\n".to_vec(),
        JobKind::Move => b"move\n".to_vec(),
    };
    for p in paths {
        data.extend_from_slice(cwd.join(p).as_os_str().as_bytes());
        data.push(0);
    }
    let dest = path();
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    // Write then rename, so a paste never sees half a list.
    let tmp = dest.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(tmp, dest)
}

pub fn load() -> Option<(JobKind, Vec<PathBuf>)> {
    let data = fs::read(path()).ok()?;
    let (head, rest) = data.split_at(data.iter().position(|&b| b == b'\n')? + 1);
    let kind = match head {
        b"copy\n" => JobKind::Copy,
        b"move\n" => JobKind::Move,
        _ => return None,
    };
    let paths: Vec<PathBuf> = rest
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(OsStr::from_bytes(p)))
        .collect();
    (!paths.is_empty()).then_some((kind, paths))
}

/// A cut is used up by its paste, as in every file manager.
pub fn clear() {
    let _ = fs::remove_file(path());
}
