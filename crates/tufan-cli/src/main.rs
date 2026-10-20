//! `tufan` command-line front end.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tufan_core::journal::{self, Journal};
use tufan_core::{
    ConflictPolicy, IntegrityMode, JobHandle, JobKind, JobSpec, JobState, LinkPolicy, Options,
};

const USAGE: &str = "\
Usage:
  tufan copy [options] <source>... <dest-dir>
  tufan move [options] <source>... <dest-dir>
  tufan resume <job-id>        Resume an interrupted job or retry failed items
  tufan jobs                   List recorded jobs
  tufan forget <job-id>        Remove a job from the journal

Options:
  --verify fast|verified|durable   Integrity mode (default: verified)
  --conflict fail|skip|rename|overwrite   (default: fail)
  --links copy-link|skip           Symbolic links (default: copy-link)
  --workers N                      Parallel file workers
  --journal PATH                   Journal database location
  -q, --quiet                      No progress output
";

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("tufan: {msg}");
            ExitCode::from(2)
        }
    }
}

fn run(args: Vec<String>) -> Result<ExitCode, String> {
    let mut opts = Options::default();
    let mut journal_path = journal::default_path();
    let mut quiet = false;
    let mut positional = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(ExitCode::SUCCESS);
            }
            "--verify" => opts.integrity = IntegrityMode::parse(&val(&a)?).ok_or("bad --verify")?,
            "--conflict" => {
                opts.conflict = ConflictPolicy::parse(&val(&a)?).ok_or("bad --conflict")?
            }
            "--links" => opts.links = LinkPolicy::parse(&val(&a)?).ok_or("bad --links")?,
            "--workers" => {
                opts.workers = val(&a)?.parse().map_err(|_| "bad --workers")?;
                opts.workers = opts.workers.clamp(1, 256);
            }
            "--journal" => journal_path = PathBuf::from(val(&a)?),
            "-q" | "--quiet" => quiet = true,
            "--" => positional.extend(it.by_ref()),
            s if s.starts_with('-') && s.len() > 1 => {
                return Err(format!("unknown option {s}\n{USAGE}"))
            }
            _ => positional.push(a),
        }
    }
    let Some(cmd) = positional.first().cloned() else {
        return Err(USAGE.to_string());
    };
    let journal = Arc::new(Journal::open(&journal_path).map_err(|e| e.to_string())?);
    let rest = &positional[1..];

    let handle = match cmd.as_str() {
        "copy" | "cp" | "move" | "mv" => {
            if rest.len() < 2 {
                return Err(USAGE.to_string());
            }
            let kind = if cmd.starts_with('c') {
                JobKind::Copy
            } else {
                JobKind::Move
            };
            let (dest, sources) = rest.split_last().unwrap();
            let sources = sources
                .iter()
                .map(|s| absolute(s))
                .collect::<Result<_, _>>()?;
            let spec = JobSpec {
                kind,
                sources,
                dest_dir: absolute(dest)?,
                options: opts,
            };
            tufan_core::start(journal, spec).map_err(|e| e.to_string())?
        }
        "resume" => {
            let id = parse_id(rest)?;
            tufan_core::resume(journal, id).map_err(|e| e.to_string())?
        }
        "jobs" => {
            for j in journal.list_jobs().map_err(|e| e.to_string())? {
                let failed = journal.failed_items(j.id).unwrap_or(0);
                println!(
                    "{:>5}  {:<5} {:<20} {} -> {}{}",
                    j.id,
                    j.spec.kind.as_str(),
                    j.state.as_str(),
                    j.spec
                        .sources
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                    j.spec.dest_dir.display(),
                    if failed > 0 {
                        format!("  ({failed} failed)")
                    } else {
                        String::new()
                    }
                );
            }
            return Ok(ExitCode::SUCCESS);
        }
        "forget" => {
            journal
                .delete_job(parse_id(rest)?)
                .map_err(|e| e.to_string())?;
            return Ok(ExitCode::SUCCESS);
        }
        other => return Err(format!("unknown command {other}\n{USAGE}")),
    };
    Ok(follow(&handle, quiet))
}

fn parse_id(rest: &[String]) -> Result<i64, String> {
    rest.first()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "expected a job id".to_string())
}

fn absolute(s: &str) -> Result<PathBuf, String> {
    std::path::absolute(s).map_err(|e| format!("{s}: {e}"))
}

fn follow(h: &JobHandle, quiet: bool) -> ExitCode {
    let tty = std::io::stderr().is_terminal() && !quiet;
    let started = Instant::now();
    let mut last = (Instant::now(), 0u64);
    let mut speed = 0f64;
    while tty && !h.is_finished() {
        std::thread::sleep(Duration::from_millis(250));
        let s = h.snapshot();
        let dt = last.0.elapsed().as_secs_f64();
        if dt > 0.0 {
            let inst = s.bytes_done.saturating_sub(last.1) as f64 / dt;
            speed = if speed == 0.0 {
                inst
            } else {
                speed * 0.8 + inst * 0.2
            };
        }
        last = (Instant::now(), s.bytes_done);
        let total = if s.scan_complete {
            format!("{}", s.files_discovered)
        } else {
            format!("{}+", s.files_discovered)
        };
        let eta = if s.scan_complete && speed > 1.0 && s.bytes_known > s.bytes_done {
            format!(
                "  ETA {}",
                fmt_dur((s.bytes_known - s.bytes_done) as f64 / speed)
            )
        } else {
            String::new()
        };
        eprint!(
            "\r\x1b[2K[{}] {}/{} files  {}/{}  {}/s{}",
            h.state().as_str(),
            s.files_done + s.files_skipped,
            total,
            fmt_bytes(s.bytes_done),
            fmt_bytes(s.bytes_known),
            fmt_bytes(speed as u64),
            eta
        );
        let _ = std::io::stderr().flush();
    }
    let state = h.wait();
    let s = h.snapshot();
    let r = h.report();
    if tty {
        eprintln!();
    }
    for e in &r.errors {
        eprintln!("error: {e}");
    }
    if r.errors_truncated {
        eprintln!("(further errors omitted)");
    }
    if !quiet {
        eprintln!(
            "job {}: {} — {} done, {} skipped, {} failed, {} in {}",
            h.id(),
            state.as_str(),
            s.files_done,
            s.files_skipped,
            s.files_failed,
            fmt_bytes(s.bytes_done),
            fmt_dur(started.elapsed().as_secs_f64())
        );
        if r.metadata_not_preserved > 0 {
            eprintln!(
                "note: some metadata could not be preserved on {} files",
                r.metadata_not_preserved
            );
        }
        if s.files_failed > 0 {
            eprintln!("retry failed items with: tufan resume {}", h.id());
        }
    }
    match state {
        JobState::Completed => ExitCode::SUCCESS,
        _ => ExitCode::from(1),
    }
}

fn fmt_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn fmt_dur(secs: f64) -> String {
    let s = secs.round() as u64;
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, s / 60 % 60)
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}
