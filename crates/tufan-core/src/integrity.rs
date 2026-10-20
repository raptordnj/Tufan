//! Streaming SHA-256 helpers. Files are never loaded whole into memory.

use std::io::{self, Read};

use sha2::{Digest, Sha256};

pub type Hasher = Sha256;

pub fn digest(h: &Hasher) -> Vec<u8> {
    h.clone().finalize().to_vec()
}

/// Hash exactly `len` bytes from `r` (or until EOF), returning the hasher and
/// the number of bytes actually read.
pub fn hash_prefix<R: Read>(r: &mut R, len: u64, buf: &mut [u8]) -> io::Result<(Hasher, u64)> {
    let mut h = Sha256::new();
    let mut left = len;
    while left > 0 {
        let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        let n = match r.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        h.update(&buf[..n]);
        left -= n as u64;
    }
    Ok((h, len - left))
}

pub fn hash_all<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<(Vec<u8>, u64)> {
    let (h, n) = hash_prefix(r, u64::MAX, buf)?;
    Ok((h.finalize().to_vec(), n))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
