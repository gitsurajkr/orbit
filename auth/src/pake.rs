//! The 6-digit code check (SPAKE2 over Ed25519), tied to both PCs' fingerprints.
//!
//! ```text
//! viewer                                         sharing PC
//!   start(code, viewer_fp, sharing_fp) ── msg ──►  respond(code, viewer_fp, sharing_fp, msg)
//!                                      ◄── msg ──
//!   finish(msg) → key                              → key
//!   proof_from_viewer(key) ──────────────────────► checks it (wrong code → stop)
//!                          ◄────────────────────── proof_from_sharer(key)
//!   checks it
//! ```
//!
//! Both sides end with the same key only if they used the same code *and* see the same two
//! fingerprints. The code itself never travels. Someone in the middle has different
//! certificates than the real PCs, so their keys never match, and each attempt is one online
//! guess (the sharing PC throws the code away after a few wrong ones).

use super::Fingerprint;
use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

pub struct Key(Vec<u8>);

fn proof(key: &Key, label: &[u8]) -> [u8; 32] {
    let mut m = Hmac::<Sha256>::new_from_slice(&key.0).expect("hmac takes any key length");
    m.update(label);
    m.finalize().into_bytes().into()
}

fn verify(key: &Key, label: &[u8], got: &[u8]) -> bool {
    let mut m = Hmac::<Sha256>::new_from_slice(&key.0).expect("hmac takes any key length");
    m.update(label);
    m.verify_slice(got).is_ok() // constant time
}

const FROM_VIEWER: &[u8] = b"orbit pairing: viewer knows the code";
const FROM_SHARER: &[u8] = b"orbit pairing: sharing PC knows the code";

impl Key {
    pub fn proof_from_viewer(&self) -> [u8; 32] {
        proof(self, FROM_VIEWER)
    }
    pub fn proof_from_sharer(&self) -> [u8; 32] {
        proof(self, FROM_SHARER)
    }
    pub fn check_viewer(&self, got: &[u8]) -> bool {
        verify(self, FROM_VIEWER, got)
    }
    pub fn check_sharer(&self, got: &[u8]) -> bool {
        verify(self, FROM_SHARER, got)
    }
}

/// Viewer side, waiting for the sharing PC's message.
pub struct Viewer(Spake2<Ed25519Group>);

/// Viewer: first message.
pub fn start(code: &str, viewer_fp: &Fingerprint, sharing_fp: &Fingerprint) -> (Viewer, Vec<u8>) {
    let (s, msg) = Spake2::<Ed25519Group>::start_a(&Password::new(code.trim()), &Identity::new(viewer_fp), &Identity::new(sharing_fp));
    (Viewer(s), msg)
}

impl Viewer {
    pub fn finish(self, sharer_msg: &[u8]) -> Result<Key> {
        self.0.finish(sharer_msg).map(Key).map_err(|e| anyhow!("pairing message: {e:?}"))
    }
}

/// Sharing PC: answers the viewer's first message, ending with its key.
pub fn respond(code: &str, viewer_fp: &Fingerprint, sharing_fp: &Fingerprint, viewer_msg: &[u8]) -> Result<(Vec<u8>, Key)> {
    let (s, msg) = Spake2::<Ed25519Group>::start_b(&Password::new(code.trim()), &Identity::new(viewer_fp), &Identity::new(sharing_fp));
    let key = s.finish(viewer_msg).map_err(|e| anyhow!("pairing message: {e:?}"))?;
    Ok((msg, Key(key)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(viewer_code: &str, sharer_code: &str, viewer_sees: Fingerprint, sharer_sees: Fingerprint) -> (bool, bool) {
        let (v, m1) = start(viewer_code, &viewer_sees, &[2; 32]);
        let (m2, sk) = respond(sharer_code, &sharer_sees, &[2; 32], &m1).unwrap();
        let vk = v.finish(&m2).unwrap();
        (sk.check_viewer(&vk.proof_from_viewer()), vk.check_sharer(&sk.proof_from_sharer()))
    }

    #[test]
    fn same_code_and_fingerprints_agree() {
        assert_eq!(run("123456", "123456", [1; 32], [1; 32]), (true, true));
    }

    #[test]
    fn wrong_code_fails() {
        assert_eq!(run("123457", "123456", [1; 32], [1; 32]), (false, false));
    }

    #[test]
    fn man_in_the_middle_fails_even_with_the_right_code() {
        // The sharing PC sees a different viewer certificate than the real viewer has.
        assert_eq!(run("123456", "123456", [1; 32], [9; 32]), (false, false));
    }

    #[test]
    fn proofs_are_not_interchangeable() {
        let (v, m1) = start("1", &[1; 32], &[2; 32]);
        let (m2, sk) = respond("1", &[1; 32], &[2; 32], &m1).unwrap();
        let vk = v.finish(&m2).unwrap();
        assert!(!sk.check_viewer(&sk.proof_from_sharer()), "a reflected proof must not pass");
        assert!(vk.check_sharer(&sk.proof_from_sharer()));
    }
}
