//! Platform-specific filesystem primitives with safe fallbacks.

use std::fs::{self, File, Metadata};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

/// Identity of a source file used to detect modification during transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceId {
    pub size: u64,
    pub mtime_ns: i128,
    pub dev: u64,
    pub ino: u64,
}

impl SourceId {
    pub fn of(md: &Metadata) -> Self {
        let mtime_ns = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(0);
        let (dev, ino) = dev_ino(md);
        SourceId {
            size: md.len(),
            mtime_ns,
            dev,
            ino,
        }
    }

    pub fn encode(&self) -> String {
        format!("{}:{}:{}:{}", self.size, self.mtime_ns, self.dev, self.ino)
    }

    pub fn decode(s: &str) -> Option<Self> {
        let mut it = s.split(':');
        let id = SourceId {
            size: it.next()?.parse().ok()?,
            mtime_ns: it.next()?.parse().ok()?,
            dev: it.next()?.parse().ok()?,
            ino: it.next()?.parse().ok()?,
        };
        it.next().is_none().then_some(id)
    }
}

#[cfg(unix)]
fn dev_ino(md: &Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (md.dev(), md.ino())
}

#[cfg(not(unix))]
fn dev_ino(_md: &Metadata) -> (u64, u64) {
    (0, 0)
}

/// Device id of the filesystem containing `path` (None when unknown).
pub fn device_of(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).ok().map(|m| m.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Rename `from` to `to` without replacing an existing `to`.
/// Returns `AlreadyExists` if the destination exists.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let f = CString::new(from.as_os_str().as_bytes())?;
        let t = CString::new(to.as_os_str().as_bytes())?;
        // SAFETY: valid NUL-terminated paths; AT_FDCWD for both directories.
        let r = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                f.as_ptr(),
                libc::AT_FDCWD,
                t.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if r == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            // Filesystem or kernel lacks RENAME_NOREPLACE: fall through.
            Some(libc::EINVAL) | Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP) => {}
            _ => return Err(e),
        }
    }
    // Portable fallback: hard_link fails atomically if `to` exists. Where hard
    // links are unsupported (FAT/exFAT, some network shares) use an
    // exists-check + rename, which has a documented TOCTOU window.
    match fs::hard_link(from, to) {
        Ok(()) => fs::remove_file(from),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
        Err(_) => {
            if fs::symlink_metadata(to).is_ok() {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            fs::rename(from, to)
        }
    }
}

/// fsync a directory so a rename inside it is durable (no-op where unsupported).
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Copy permissions and modification time from source metadata.
/// Returns a list of metadata kinds that could not be preserved.
pub fn apply_metadata(file: &File, src: &Metadata) -> Vec<&'static str> {
    let mut lost = Vec::new();
    if file.set_permissions(src.permissions()).is_err() {
        lost.push("permissions");
    }
    if let Ok(m) = src.modified() {
        let mut times = fs::FileTimes::new().set_modified(m);
        if let Ok(a) = src.accessed() {
            times = times.set_accessed(a);
        }
        if file.set_times(times).is_err() {
            lost.push("timestamps");
        }
    }
    lost
}

/// Lexically normalise an absolute path (resolves `.` and `..`).
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// True if `inner` equals or lies under `outer`, after canonicalisation
/// where possible.
pub fn is_within(inner: &Path, outer: &Path) -> bool {
    let canon = |p: &Path| -> PathBuf {
        fs::canonicalize(p).unwrap_or_else(|_| {
            // Destination may not exist yet: canonicalise the nearest ancestor.
            let mut tail = Vec::new();
            let mut cur = p.to_path_buf();
            loop {
                if let Ok(c) = fs::canonicalize(&cur) {
                    let mut r = c;
                    for t in tail.iter().rev() {
                        r.push(t);
                    }
                    return normalize(&r);
                }
                match (cur.file_name().map(|s| s.to_os_string()), cur.parent()) {
                    (Some(name), Some(parent)) => {
                        tail.push(name);
                        cur = parent.to_path_buf();
                    }
                    _ => return normalize(p),
                }
            }
        })
    };
    let (i, o) = (canon(inner), canon(outer));
    i.starts_with(&o)
}

/// If `path` exists but the directory stores it under a different spelling
/// (case-insensitive or normalising filesystem), return that spelling.
pub fn existing_spelling_differs(path: &Path) -> bool {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    let Ok(rd) = fs::read_dir(parent) else {
        return false;
    };
    for e in rd.flatten() {
        if e.file_name() == name {
            return false;
        }
    }
    // Exists (caller checked) but no exact-name entry: equivalent name differs.
    true
}

/// `name (n).ext` candidates for the Rename conflict policy.
pub fn renamed_candidate(path: &Path, n: u32) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match path.extension() {
        Some(ext) if !stem.is_empty() => format!("{stem} ({n}).{}", ext.to_string_lossy()),
        _ => format!(
            "{} ({n})",
            path.file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default()
        ),
    };
    path.with_file_name(name)
}

/// Deterministic temporary name for a destination file within a job.
pub fn temp_path(dest: &Path, job_id: i64) -> PathBuf {
    let name = dest
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    dest.with_file_name(format!(".{name}.tufan-{job_id}.part"))
}

pub fn is_temp_for_job(name: &str, job_id: i64) -> bool {
    name.starts_with('.') && name.ends_with(&format!(".tufan-{job_id}.part"))
}

#[cfg(unix)]
pub fn make_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
pub fn make_symlink(target: &Path, link: &Path) -> io::Result<()> {
    let is_dir = fs::metadata(link.parent().unwrap_or(Path::new(".")).join(target))
        .map(|m| m.is_dir())
        .unwrap_or(false);
    if is_dir {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}
