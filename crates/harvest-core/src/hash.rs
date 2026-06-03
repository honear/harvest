//! Checksum abstraction: a blazing-fast default (xxHash3) plus an
//! industry-standard alternative (MD5) for interop with other media tools.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result};
use md5::{Digest, Md5};
use xxhash_rust::xxh3::Xxh3;
use xxhash_rust::xxh64::Xxh64;

/// Which checksum algorithm to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    /// xxHash64 — very fast and the media-industry / MHL standard. Default.
    Xxh64,
    /// xxHash3 (64-bit) — even faster, but not part of the classic MHL spec.
    Xxh3,
    /// MD5 — slower, but the de-facto standard for media hash lists / sidecars.
    Md5,
}

impl HashAlgo {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "xxh64" | "xxhash64" => Some(Self::Xxh64),
            "xxh3" | "xxhash3" => Some(Self::Xxh3),
            "md5" => Some(Self::Md5),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Xxh64 => "xxh64",
            Self::Xxh3 => "xxh3",
            Self::Md5 => "md5",
        }
    }

    /// The element name used for this algorithm in a classic MHL hash list,
    /// or `None` if MHL has no element for it (so a sidecar is used instead).
    pub fn mhl_element(self) -> Option<&'static str> {
        match self {
            // Hex of the u64 is already big-endian, matching MHL's xxhash64be.
            Self::Xxh64 => Some("xxhash64be"),
            Self::Md5 => Some("md5"),
            Self::Xxh3 => None,
        }
    }
}

/// A streaming hasher that can be fed bytes incrementally.
pub enum Hasher {
    Xxh64(Box<Xxh64>),
    Xxh3(Box<Xxh3>),
    Md5(Md5),
}

impl Hasher {
    pub fn new(algo: HashAlgo) -> Self {
        match algo {
            HashAlgo::Xxh64 => Hasher::Xxh64(Box::new(Xxh64::new(0))),
            HashAlgo::Xxh3 => Hasher::Xxh3(Box::new(Xxh3::new())),
            HashAlgo::Md5 => Hasher::Md5(Md5::new()),
        }
    }

    #[inline]
    pub fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Xxh64(h) => h.update(data),
            Hasher::Xxh3(h) => h.update(data),
            Hasher::Md5(h) => h.update(data),
        }
    }

    pub fn finalize_hex(self) -> String {
        match self {
            Hasher::Xxh64(h) => format!("{:016x}", h.digest()),
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
    fn xxh64_known_answer() {
        // xxHash64 of empty input with seed 0 (canonical test vector).
        assert_eq!(hash_bytes(HashAlgo::Xxh64, b""), "ef46db3751d8e999");
    }

    #[test]
    fn fast_hashes_are_deterministic_and_content_sensitive() {
        for algo in [HashAlgo::Xxh64, HashAlgo::Xxh3] {
            assert_eq!(hash_bytes(algo, b"abc"), hash_bytes(algo, b"abc"));
            assert_ne!(hash_bytes(algo, b"abc"), hash_bytes(algo, b"abd"));
        }
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
