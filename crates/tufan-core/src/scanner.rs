//! Incremental directory traversal feeding a bounded channel.
//!
//! The tree is never collected in memory: entries are produced as they are
//! read and the scanner blocks when workers fall behind. Only the stack of
//! not-yet-visited directories is held.

use std::fs::{self, Metadata};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;

use crate::control::{Control, Progress};
use crate::error::{Error, Result};

#[derive(Debug)]
pub enum Item {
    File { src: PathBuf, dest: PathBuf, md: Metadata },
    Symlink { src: PathBuf, dest: PathBuf },
    Failed { path: PathBuf, error: Error },
}

pub struct ScanOutput {
    /// Source directories, parents before children (for bottom-up removal
    /// after a move).
    pub dirs: Vec<PathBuf>,
}

/// Walk `sources`, creating destination directories and emitting items.
/// Symbolic links are reported, never followed.
pub fn scan(
    sources: &[PathBuf],
    dest_dir: &PathBuf,
    tx: &SyncSender<Item>,
    control: &Control,
    progress: &Progress,
    record_dirs: bool,
) -> Result<ScanOutput> {
    let mut out = ScanOutput { dirs: Vec::new() };
    let send = |item: Item| -> Result<()> {
        if let Item::File { md, .. } = &item {
            progress.files_discovered.fetch_add(1, Ordering::Relaxed);
            progress.bytes_known.fetch_add(md.len(), Ordering::Relaxed);
        } else if let Item::Symlink { .. } = &item {
            progress.files_discovered.fetch_add(1, Ordering::Relaxed);
        }
        tx.send(item).map_err(|_| Error::Cancelled)
    };

    for src in sources {
        control.checkpoint()?;
        let Some(name) = src.file_name() else {
            send(Item::Failed { path: src.clone(), error: Error::InvalidPath(src.clone()) })?;
            continue;
        };
        let dest = dest_dir.join(name);
        let md = match fs::symlink_metadata(src) {
            Ok(m) => m,
            Err(e) => {
                send(Item::Failed { path: src.clone(), error: Error::io(src, e) })?;
                continue;
            }
        };
        if md.file_type().is_symlink() {
            send(Item::Symlink { src: src.clone(), dest })?;
            continue;
        }
        if md.is_file() {
            send(Item::File { src: src.clone(), dest, md })?;
            continue;
        }
        if !md.is_dir() {
            send(Item::Failed { path: src.clone(), error: Error::InvalidPath(src.clone()) })?;
            continue;
        }

        let mut stack = vec![(src.clone(), dest)];
        while let Some((dir, dest)) = stack.pop() {
            control.checkpoint()?;
            if let Err(e) = ensure_dir(&dir, &dest) {
                send(Item::Failed { path: dir.clone(), error: e })?;
                continue;
            }
            let rd = match fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(e) => {
                    send(Item::Failed { path: dir.clone(), error: Error::io(&dir, e) })?;
                    continue;
                }
            };
            if record_dirs {
                out.dirs.push(dir.clone());
            }
            for entry in rd {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        send(Item::Failed { path: dir.clone(), error: Error::io(&dir, e) })?;
                        continue;
                    }
                };
                let path = entry.path();
                let target = dest.join(entry.file_name());
                // DirEntry::file_type does not follow symlinks.
                let ft = match entry.file_type() {
                    Ok(t) => t,
                    Err(e) => {
                        send(Item::Failed { path: path.clone(), error: Error::io(&path, e) })?;
                        continue;
                    }
                };
                if ft.is_symlink() {
                    send(Item::Symlink { src: path, dest: target })?;
                } else if ft.is_dir() {
                    stack.push((path, target));
                } else if ft.is_file() {
                    match entry.metadata() {
                        Ok(md) => send(Item::File { src: path, dest: target, md })?,
                        Err(e) => send(Item::Failed { path: path.clone(), error: Error::io(&path, e) })?,
                    }
                } else {
                    // Sockets, FIFOs, devices: not copied.
                    send(Item::Failed { path: path.clone(), error: Error::InvalidPath(path) })?;
                }
            }
        }
    }
    progress.scan_complete.store(1, Ordering::Relaxed);
    Ok(out)
}

fn ensure_dir(src: &PathBuf, dest: &PathBuf) -> Result<()> {
    match fs::create_dir(dest) {
        Ok(()) => {
            // Read-only directory modes would block writing children; those
            // are left at the default (documented limitation).
            if let Ok(md) = fs::metadata(src) {
                if !md.permissions().readonly() {
                    let _ = fs::set_permissions(dest, md.permissions());
                }
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match fs::symlink_metadata(dest) {
            // Merge into an existing real directory; never descend through a link.
            Ok(m) if m.is_dir() => Ok(()),
            _ => Err(Error::Conflict(dest.clone())),
        },
        Err(e) => Err(Error::io(dest, e)),
    }
}
