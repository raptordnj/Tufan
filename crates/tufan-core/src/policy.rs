use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrityMode {
    /// Size and source-change checks only. Not a content guarantee.
    Fast,
    /// SHA-256 of the source stream compared with a re-read of the output.
    Verified,
    /// Verified, plus fsync of file and directory before commit.
    DurableVerified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    Skip,
    Overwrite,
    Rename,
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkPolicy {
    /// Recreate the symbolic link itself (never follow it).
    CopyLink,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Copy,
    Move,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub integrity: IntegrityMode,
    pub conflict: ConflictPolicy,
    pub links: LinkPolicy,
    pub workers: usize,
    pub queue_depth: usize,
    pub buffer_size: usize,
    pub max_retries: u32,
}

impl Default for Options {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Options {
            integrity: IntegrityMode::Verified,
            conflict: ConflictPolicy::Fail,
            links: LinkPolicy::CopyLink,
            workers: cpus.clamp(2, 16),
            queue_depth: 1024,
            buffer_size: 1 << 20,
            max_retries: 5,
        }
    }
}

#[derive(Debug, Clone)]
pub struct JobSpec {
    pub kind: JobKind,
    pub sources: Vec<PathBuf>,
    pub dest_dir: PathBuf,
    pub options: Options,
}

impl IntegrityMode {
    pub fn as_str(self) -> &'static str {
        match self {
            IntegrityMode::Fast => "fast",
            IntegrityMode::Verified => "verified",
            IntegrityMode::DurableVerified => "durable",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "fast" => IntegrityMode::Fast,
            "verified" => IntegrityMode::Verified,
            "durable" => IntegrityMode::DurableVerified,
            _ => return None,
        })
    }
    pub fn hashes(self) -> bool {
        self != IntegrityMode::Fast
    }
    pub fn durable(self) -> bool {
        self == IntegrityMode::DurableVerified
    }
}

impl ConflictPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictPolicy::Skip => "skip",
            ConflictPolicy::Overwrite => "overwrite",
            ConflictPolicy::Rename => "rename",
            ConflictPolicy::Fail => "fail",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "skip" => ConflictPolicy::Skip,
            "overwrite" => ConflictPolicy::Overwrite,
            "rename" => ConflictPolicy::Rename,
            "fail" => ConflictPolicy::Fail,
            _ => return None,
        })
    }
}

impl LinkPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkPolicy::CopyLink => "copy-link",
            LinkPolicy::Skip => "skip",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "copy-link" => LinkPolicy::CopyLink,
            "skip" => LinkPolicy::Skip,
            _ => return None,
        })
    }
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            JobKind::Copy => "copy",
            JobKind::Move => "move",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "copy" => JobKind::Copy,
            "move" => JobKind::Move,
            _ => return None,
        })
    }
}
