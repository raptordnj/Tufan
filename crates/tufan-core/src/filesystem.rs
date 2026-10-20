//! Platform-specific filesystem primitives with safe fallbacks.

use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

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
    // MoveFileExW without MOVEFILE_REPLACE_EXISTING is atomic and refuses
    // to replace. Unlike link + unlink it has no second step that antivirus
    // or indexers can block after the destination already exists.
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
        use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
        let wide = |p: &Path| -> io::Result<Vec<u16>> {
            Ok(verbatim(p)?
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect())
        };
        let (f, t) = (wide(from)?, wide(to)?);
        // SAFETY: both are NUL-terminated UTF-16 paths.
        if unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), 0) } != 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        return match e.raw_os_error().map(|c| c as u32) {
            Some(ERROR_ALREADY_EXISTS) | Some(ERROR_FILE_EXISTS) => {
                Err(io::Error::from(io::ErrorKind::AlreadyExists))
            }
            _ => Err(e),
        };
    }
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
    #[allow(unreachable_code)]
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

/// `\\?\` form of an absolute path, so Win32 calls accept paths longer
/// than MAX_PATH. The standard library does this itself for its own calls.
#[cfg(windows)]
fn verbatim(p: &Path) -> io::Result<PathBuf> {
    let abs = std::path::absolute(p)?;
    let s = abs.as_os_str().to_string_lossy();
    Ok(if s.starts_with(r"\\?\") {
        abs
    } else if let Some(unc) = s.strip_prefix(r"\\") {
        PathBuf::from(format!(r"\\?\UNC\{unc}"))
    } else {
        PathBuf::from(format!(r"\\?\{s}"))
    })
}

/// Plain form of a `\\?\` path (as returned by `read_link` on Windows).
pub fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.as_os_str().to_string_lossy();
    if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{unc}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\").or_else(|| s.strip_prefix(r"\??\")) {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
}

/// Clear the read-only attribute (Windows refuses to replace or delete
/// read-only files; elsewhere it does not apply).
pub fn make_writable(p: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let mut perm = fs::symlink_metadata(p)?.permissions();
        if perm.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perm.set_readonly(false);
            fs::set_permissions(p, perm)?;
        }
    }
    let _ = p;
    Ok(())
}

/// `remove_file` that also removes read-only files on Windows.
pub fn remove_file_force(p: &Path) -> io::Result<()> {
    match fs::remove_file(p) {
        Err(e) if cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied => {
            make_writable(p)?;
            fs::remove_file(p)
        }
        r => r,
    }
}

/// fsync an existing file by path. Windows only flushes through a handle
/// with write access, which a read-only file refuses until the attribute is
/// cleared; it is restored afterwards.
pub fn sync_file(p: &Path) -> io::Result<()> {
    if !cfg!(windows) {
        return File::open(p)?.sync_all();
    }
    let open = || fs::OpenOptions::new().write(true).open(p);
    match open() {
        Ok(f) => f.sync_all(),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            let perm = fs::symlink_metadata(p)?.permissions();
            if !perm.readonly() {
                return Err(e);
            }
            make_writable(p)?;
            let r = open().and_then(|f| f.sync_all());
            fs::set_permissions(p, perm)?;
            r
        }
        Err(e) => Err(e),
    }
}

/// True if both paths name the same file (any spelling, link or mount).
pub fn same_file(a: &Path, b: &Path) -> bool {
    same_file::is_same_file(a, b).unwrap_or(false)
}

/// Byte-for-byte comparison of two regular files of equal length.
pub fn same_content(a: &Path, b: &Path, buf: &mut Vec<u8>) -> io::Result<bool> {
    let (mut fa, mut fb) = (File::open(a)?, File::open(b)?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    if buf.len() < 2 {
        buf.resize(1 << 20, 0);
    }
    let half = buf.len() / 2;
    let (ba, bb) = buf.split_at_mut(half);
    loop {
        let n = read_full(&mut fa, ba)?;
        let m = read_full(&mut fb, &mut bb[..half])?;
        if n != m || ba[..n] != bb[..n] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
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

/// Deterministic temporary name for one item of a job. It is derived from
/// the source key, not the destination name, so names that differ only in
/// case never share a temp file on case-insensitive filesystems, and it is
/// short enough not to exceed name-length limits.
pub fn temp_path(dest: &Path, job_id: i64, key: &[u8]) -> PathBuf {
    let h = Sha256::digest(key);
    let tag: String = h[..8].iter().map(|b| format!("{b:02x}")).collect();
    dest.with_file_name(format!(".tufan-{job_id}-{tag}.part"))
}

pub fn is_temp_for_job(name: &str, job_id: i64) -> bool {
    // Current names, and `.<name>.tufan-<job>.part` from version 0.1.0.
    (name.starts_with(&format!(".tufan-{job_id}-")) && name.ends_with(".part"))
        || (name.starts_with('.') && name.ends_with(&format!(".tufan-{job_id}.part")))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    File,
    Dir,
    /// Windows directory junction (mount point).
    Junction,
}

/// Kind of the link at `path`, taken from the link itself: its target may
/// not exist (yet) at the destination.
pub fn link_kind(path: &Path) -> LinkKind {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        if junction::exists(path).unwrap_or(false) {
            return LinkKind::Junction;
        }
        if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink_dir()) {
            return LinkKind::Dir;
        }
    }
    let _ = path;
    LinkKind::File
}

/// Point an absolute link target that lies inside one of the job's source
/// roots at the matching place in the destination, so copied trees do not
/// link back into the original (and moved ones do not dangle). Relative
/// targets already move with the tree.
pub fn retarget(target: &Path, sources: &[PathBuf], dest_dir: &Path) -> PathBuf {
    if !target.is_absolute() {
        return target.to_path_buf();
    }
    let plain = strip_verbatim(target);
    for root in sources {
        let Some(name) = root.file_name() else {
            continue;
        };
        // Link targets are stored in long form; the root may be spelled
        // differently (relative, 8.3 short names), so try both forms.
        let forms = [std::path::absolute(root).ok(), fs::canonicalize(root).ok()];
        for form in forms.into_iter().flatten() {
            if let Ok(rest) = plain.strip_prefix(strip_verbatim(&form)) {
                return dest_dir.join(name).join(rest);
            }
        }
    }
    target.to_path_buf()
}

/// Create a link of `kind` at `link`. On Windows, directory symlinks need a
/// privilege most accounts lack; a junction (no privilege needed) is used
/// instead. File symlinks report the error so the caller can copy content.
pub fn make_link(target: &Path, link: &Path, kind: LinkKind) -> io::Result<()> {
    #[cfg(unix)]
    {
        let _ = kind;
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        // Junctions need an absolute target.
        let absolute = || {
            let t = strip_verbatim(target);
            if t.is_absolute() {
                t
            } else {
                normalize(&link.parent().unwrap_or(Path::new(".")).join(t))
            }
        };
        match kind {
            LinkKind::Junction => junction::create(absolute(), link),
            LinkKind::Dir => match symlink_dir(target, link) {
                // ERROR_PRIVILEGE_NOT_HELD
                Err(e) if e.raw_os_error() == Some(1314) => junction::create(absolute(), link),
                r => r,
            },
            LinkKind::File => symlink_file(target, link),
        }
    }
}

/// Remove a link without touching its target.
pub fn remove_link(path: &Path) -> io::Result<()> {
    match link_kind(path) {
        LinkKind::Dir | LinkKind::Junction => fs::remove_dir(path),
        LinkKind::File => remove_file_force(path),
    }
}

/// True for the Windows error a file symlink fails with when the account
/// lacks the symlink privilege.
pub fn is_privilege_error(e: &io::Error) -> bool {
    cfg!(windows) && e.raw_os_error() == Some(1314)
}
