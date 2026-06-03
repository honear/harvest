//! Checksum abstraction: a blazing-fast default (xxHash3) plus an
//! industry-standard alternative (MD5) for interop with other media tools.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result};
use md5::{Digest, Md5};
use xxhash_rust::xxh3::Xxh3;

/// Which checksum algorithm to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    /// xxHash3 (64-bit) — extremely fast, non-cryptographic integrity check.
    Xxh3,
    /// MD5 — slower, but the de-facto standard for media hash lists / sidecars.
    Md5,
}

impl HashAlgo {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "xxh3" | "xxhash" | "xxhash3" => Some(Self::Xxh3),
            "md5" => Some(Self::Md5),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Xxh3 => "xxh3",
            Self::Md5 => "md5",
        }
    }
}

/// A streaming hasher that can be fed bytes incrementally.
pub enum Hasher {
    Xxh3(Box<Xxh3>),
    Md5(Md5),
}

impl Hasher {
    pub fn new(algo: HashAlgo) -> Self {
        match algo {
            HashAlgo::Xxh3 => Hasher::Xxh3(Box::new(Xxh3::new())),
            HashAlgo::Md5 => Hasher::Md5(Md5::new()),
        }
    }

    #[inline]
    pub fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Xxh3(h) => h.update(data),
            Hasher::Md5(h) => h.update(data),
        }
    }

    pub fn finalize_hex(self) -> String {
        match self {
            Hasher::Xxh3(h) => format!("{:016x}", h.digest()),
            Hasher::Md5(h) => {
                let out = h.finalize();
                let mut s = String::with_capacity(out.len() * 2);
                for b in out {
                    s.push_str(&format!("{b:02x}"));
                }
                s
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_bytes(algo: HashAlgo, data: &[u8]) -> String {
        let mut h = Hasher::new(algo);
        h.update(data);
        h.finalize_hex()
    }

    #[test]
    fn md5_known_answers() {
        // RFC 1321 test vectors.
        assert_eq!(hash_bytes(HashAlgo::Md5, b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hash_bytes(HashAlgo::Md5, b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn xxh3_is_deterministic_and_content_sensitive() {
        assert_eq!(hash_bytes(HashAlgo::Xxh3, b"abc"), hash_bytes(HashAlgo::Xxh3, b"abc"));
        assert_ne!(hash_bytes(HashAlgo::Xxh3, b"abc"), hash_bytes(HashAlgo::Xxh3, b"abd"));
    }

    #[test]
    fn streaming_matches_oneshot() {
        // Feeding bytes in chunks must equal hashing them all at once.
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let oneshot = hash_bytes(HashAlgo::Xxh3, &data);
        let mut h = Hasher::new(HashAlgo::Xxh3);
        for chunk in data.chunks(97) {
            h.update(chunk);
        }
        assert_eq!(oneshot, h.finalize_hex());
    }
}

/// Hash an entire file from disk. Used for destination read-back verification.
pub fn hash_file(path: &Path, algo: HashAlgo, buf_size: usize) -> Result<String> {
    let file = File::open(path).with_context(|| format!("opening {} to hash", path.display()))?;
    let mut reader = BufReader::with_capacity(buf_size, file);
    let mut hasher = Hasher::new(algo);
    let mut buf = vec![0u8; buf_size];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize_hex())
}
