# Tufan

Safe, resumable, high-throughput file copy and move. The Rust engine
(`tufan-core`) works without any UI. There are two front ends on top of it:
a CLI (`tufan`) and a GTK 4 desktop app (`tufan-gtk`).

Design record: `docs/superpowers/specs/2026-10-21-tufan-design.md`.
Status and remaining work: `TODO.md`.

## Build

    cargo build --release            # needs GTK 4 dev files for tufan-gtk
    cargo test --release -p tufan-core

## CLI

    tufan copy [--verify fast|verified|durable] [--conflict fail|skip|rename|overwrite] SRC... DEST_DIR
    tufan move ...                    # same options
    tufan jobs                        # list recorded jobs
    tufan resume ID                   # continue after a crash/cancel, or retry failed items

The journal lives in `$XDG_DATA_HOME/tufan` on Linux,
`~/Library/Application Support/Tufan` on macOS and `%LOCALAPPDATA%\Tufan` on
Windows. Set `TUFAN_HOME` or pass `--journal` to put it somewhere else.

## Guarantees (and their limits)

- **Publication.** Each file is written to `.<name>.tufan-<job>.part` in the
  destination folder, then renamed into place. An existing destination is
  never truncated. With `overwrite` it is replaced atomically by rename;
  otherwise Tufan uses a rename that refuses to replace
  (`renameat2(RENAME_NOREPLACE)` on Linux, a hard-link trick elsewhere).
  On filesystems without hard links (FAT/exFAT, some shares) the
  no-replace check has a small race window.
- **Integrity modes.**
  - *fast*: checks size and that the source didn't change (size, mtime,
    inode). It does **not** check content.
  - *verified*: the SHA-256 of the source stream must match a re-read of
    the temp file before publishing. The re-read may come from the page
    cache, so it catches software and transfer errors, not necessarily
    media errors.
  - *durable*: *verified*, plus an fsync of the file and its directory
    before the file is reported as committed.
- **Move.** On the same filesystem, a move is a single rename. Across
  filesystems it is copy, then verify, then publish, then a durable journal
  commit, and only then source deletion. The source is deleted only if its
  identity is unchanged. If deletion fails, the move is reported as partial
  and both copies are kept. A crash between commit and deletion is finished
  on `resume`.
- **Resume.** Job level: items that are done and whose destination is
  still present with the right size are skipped. File level: files of
  64 MiB or more checkpoint every 64 MiB (fsync plus the prefix SHA-256 in
  the journal). A checkpoint is re-hashed before it is trusted; a
  mismatch restarts the file from zero.
- **Bounded resources.** A fixed number of workers, a bounded scan queue,
  one reused buffer per worker, and journal writes batched every
  256 rows / 250 ms. The tree is never loaded into memory. (For moves, the
  list of source *directories* is kept so empty ones can be removed.)
- **Links.** Symlinks are never followed. They are recreated (default) or
  skipped. Hard links are copied as independent files.
- **Metadata.** Permissions and timestamps are preserved. Ownership, ACLs,
  xattrs, ADS and sparse layout are **not** preserved yet. Read-only
  directory modes are not applied to the destination.
- Hardware, firmware, network filesystems and tmpfs can break these
  assumptions. Durability on tmpfs and many network shares is not real.

## Benchmarks

`bench/bench.sh [dir] [files] [MiB]`. One sample run: Linux 7.2, 12 CPUs,
tmpfs, warm cache, 100k files of 64 B–4 KiB plus one 1 GiB file:

| case | time |
|---|---|
| `cp -r` small | 1.41 s |
| tufan small fast / verified / durable | 1.66 / 1.69 / 1.72 s |
| `cp` 1 GiB | 0.35 s |
| tufan 1 GiB fast / verified | 1.11 / 1.92 s |

On tmpfs, Tufan trails `cp`, which does no temp files, verification or
journaling. For large files, Fast mode still hashes so that checkpoints are
resumable. Re-run on your own storage before drawing conclusions.

## Platform status

| target | compiled | packaged | tested |
|---|---|---|---|
| Linux x86-64 (engine, CLI, GTK) | yes | PKGBUILD (not built) | yes, here |
| Windows / macOS / FreeBSD / other arches | CI config only | NSIS script compiles with stub binaries | no |

Nothing is officially supported yet.
