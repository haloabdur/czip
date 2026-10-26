//! Password protection for `.czip`: Argon2id key derivation and
//! XChaCha20-Poly1305 per block, so every block is authenticated on its own.

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

use crate::{Error, Result};

pub(crate) const SALT_LEN: usize = 16;
pub(crate) const PREFIX_LEN: usize = 16;
const AAD: &[u8] = b"czip/v1";

/// Argon2id cost: 64 MiB, 3 passes, 4 lanes.
pub(crate) const KDF_M: u32 = 64 * 1024;
pub(crate) const KDF_T: u32 = 3;
pub(crate) const KDF_P: u32 = 4;

pub(crate) struct Cipher {
    aead: XChaCha20Poly1305,
    prefix: [u8; PREFIX_LEN],
}

impl Cipher {
    pub(crate) fn derive(
        password: &str,
        salt: &[u8; SALT_LEN],
        prefix: [u8; PREFIX_LEN],
        (m, t, p): (u32, u32, u32),
    ) -> Result<Self> {
        let params = Params::new(m, t, p, Some(32))
            .map_err(|e| Error::Corrupt(format!("bad key parameters: {e}")))?;
        let mut key = [0u8; 32];
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(password.as_bytes(), salt, &mut key)
            .map_err(|e| Error::Corrupt(format!("key derivation failed: {e}")))?;
        Ok(Self { aead: XChaCha20Poly1305::new(&Key::from(key)), prefix })
    }

    fn nonce(&self, counter: u64) -> XNonce {
        let mut n = [0u8; 24];
        n[..PREFIX_LEN].copy_from_slice(&self.prefix);
        n[PREFIX_LEN..].copy_from_slice(&counter.to_le_bytes());
        XNonce::from(n)
    }

    pub(crate) fn seal(&self, counter: u64, data: &[u8]) -> Result<Vec<u8>> {
        self.aead
            .encrypt(&self.nonce(counter), Payload { msg: data, aad: AAD })
            .map_err(|_| Error::Corrupt("encryption failed".into()))
    }

    pub(crate) fn open(&self, counter: u64, data: &[u8]) -> Option<Vec<u8>> {
        self.aead.decrypt(&self.nonce(counter), Payload { msg: data, aad: AAD }).ok()
    }
}

pub(crate) fn random<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
    Ok(b)
}
