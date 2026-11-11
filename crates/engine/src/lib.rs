//! Maxtrix engine — headless core for the portable tool library.
//!
//! Pipeline: fetch a Scoop-style JSON manifest -> stream the artifact to disk while
//! hashing it -> verify -> unpack into a staging folder next to the tool ->
//! carry over user data (`persist`) -> swap it in for `Tools\<Category>\<Name>\`.

pub mod custom;
pub mod health;
pub mod i18n;
pub mod manifest;
pub mod net;
pub mod extract;
pub mod fsutil;
pub mod sevenzip;
pub mod msi;
pub mod persist;
pub mod resolver;
pub mod install;
pub mod catalog;
pub mod cli;
pub mod lists;
pub mod selfupdate;
pub mod search;
pub mod drives;

pub use install::{human, install, InstallEvent, InstallReport};
pub use manifest::{parse_manifest, Manifest};

/// Returned (inside `anyhow::Error`) when the user cancelled an install —
/// check with `err.downcast_ref::<Cancelled>()`.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(crate::tr!("cancelled by the user", "vom Benutzer abgebrochen").as_str())
    }
}

impl std::error::Error for Cancelled {}

/// Lowercase hex encoding of a byte slice.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        write!(s, "{:02x}", b).unwrap();
    }
    s
}

/// Incremental digest for streamed downloads (the four algorithms Scoop uses).
pub(crate) enum StreamHasher {
    Sha256(sha2::Sha256),
    Sha512(sha2::Sha512),
    Sha1(sha1::Sha1),
    Md5(md5::Md5),
}

impl StreamHasher {
    pub(crate) fn new(alg: &str) -> anyhow::Result<Self> {
        use sha2::Digest; // digest::Digest — also drives Sha1 / Md5
        Ok(match alg {
            "sha256" => StreamHasher::Sha256(sha2::Sha256::new()),
            "sha512" => StreamHasher::Sha512(sha2::Sha512::new()),
            "sha1" => StreamHasher::Sha1(sha1::Sha1::new()),
            "md5" => StreamHasher::Md5(md5::Md5::new()),
            other => anyhow::bail!(crate::tr!("hash algorithm '{}' is not supported", "Hash-Algorithmus '{}' wird nicht unterstützt", other)),
        })
    }

    pub(crate) fn update(&mut self, data: &[u8]) {
        use sha2::Digest;
        match self {
            StreamHasher::Sha256(h) => h.update(data),
            StreamHasher::Sha512(h) => h.update(data),
            StreamHasher::Sha1(h) => h.update(data),
            StreamHasher::Md5(h) => h.update(data),
        }
    }

    pub(crate) fn finish_hex(self) -> String {
        use sha2::Digest;
        match self {
            StreamHasher::Sha256(h) => to_hex(&h.finalize()),
            StreamHasher::Sha512(h) => to_hex(&h.finalize()),
            StreamHasher::Sha1(h) => to_hex(&h.finalize()),
            StreamHasher::Md5(h) => to_hex(&h.finalize()),
        }
    }
}

/// Compute a hex digest with the algorithm Scoop names in a manifest hash.
/// Supports the four prefixes Scoop uses: sha256 (default), sha512, sha1, md5.
pub fn compute_hash(alg: &str, data: &[u8]) -> anyhow::Result<String> {
    let mut h = StreamHasher::new(alg)?;
    h.update(data);
    Ok(h.finish_hex())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_hash_equals_one_shot_hash() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i * 31 % 251) as u8).collect();
        for alg in ["sha256", "sha512", "sha1", "md5"] {
            let mut h = StreamHasher::new(alg).unwrap();
            for chunk in data.chunks(65_536) {
                h.update(chunk);
            }
            assert_eq!(h.finish_hex(), compute_hash(alg, &data).unwrap(), "{alg}");
        }
    }

    #[test]
    fn known_answers_for_all_algorithms() {
        // FIPS 180 / RFC 1321 test vectors for "abc".
        for (alg, hex) in [
            ("sha256", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
            ("sha1", "a9993e364706816aba3e25717850c26c9cd0d89d"),
            ("md5", "900150983cd24fb0d6963f7d28e17f72"),
            ("sha512", "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"),
        ] {
            assert_eq!(compute_hash(alg, b"abc").unwrap(), hex, "{alg}");
        }
    }
}
