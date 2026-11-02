//! This installation's identity with the coordinator: an Ed25519 key created on
//! first use, with no account or sign-in. The coordinator trusts results for
//! the work they prove, not for who sent them (docs/distributed.md), so the key
//! only has to be stable, not vouched for.
//!
//! The private key is stored next to the database, encrypted with DPAPI for the
//! current Windows user.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};

const KEY_FILE: &str = "identity.key";
const REGISTRATION_FILE: &str = "coordinator.json";

pub struct Identity {
    key: SigningKey,
    dir: PathBuf,
}

/// The worker ID a coordinator assigned to this key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Registration {
    url: String,
    worker_id: String,
}

impl Identity {
    /// Loads the key from `dir`, creating it on first use.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let path = dir.join(KEY_FILE);
        let key = match std::fs::read(&path) {
            Ok(sealed) => {
                let seed = dpapi::unprotect(&sealed).with_context(|| format!("cannot decrypt {}", path.display()))?;
                let Ok(seed) = <[u8; 32]>::try_from(seed.as_slice()) else {
                    bail!("{} is corrupt", path.display());
                };
                SigningKey::from_bytes(&seed)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut seed = [0u8; 32];
                getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("no secure randomness: {e}"))?;
                std::fs::create_dir_all(dir)?;
                write_atomically(&path, &dpapi::protect(&seed)?)?;
                tracing::info!(file = %path.display(), "created this installation's coordinator identity");
                SigningKey::from_bytes(&seed)
            }
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        Ok(Self {
            key,
            dir: dir.to_path_buf(),
        })
    }

    pub fn public_key_base64(&self) -> String {
        BASE64.encode(self.key.verifying_key().as_bytes())
    }

    pub fn sign_base64(&self, message: &[u8]) -> String {
        BASE64.encode(self.key.sign(message).to_bytes())
    }

    /// The worker ID registered with the coordinator at `url`, if any.
    pub fn worker_id(&self, url: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.dir.join(REGISTRATION_FILE)).ok()?;
        let registration: Registration = serde_json::from_str(&text).ok()?;
        (registration.url == url).then_some(registration.worker_id)
    }

    pub fn save_worker_id(&self, url: &str, worker_id: &str) -> Result<()> {
        let registration = Registration {
            url: url.to_string(),
            worker_id: worker_id.to_string(),
        };
        write_atomically(
            &self.dir.join(REGISTRATION_FILE),
            serde_json::to_string_pretty(&registration)?.as_bytes(),
        )
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path).with_context(|| format!("cannot write {}", path.display()))
}

#[cfg(windows)]
mod dpapi {
    use anyhow::{Context, Result};
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    use windows::core::PCWSTR;

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    /// Copies and frees a blob that Windows allocated.
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        unsafe {
            let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
            bytes
        }
    }

    pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptProtectData(
                &blob(data),
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .context("CryptProtectData failed")?;
            Ok(take(out))
        }
    }

    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptUnprotectData(&blob(data), None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
                .context("CryptUnprotectData failed (was the key created by another Windows user?)")?;
            Ok(take(out))
        }
    }
}

/// Elsewhere the key relies on the data directory's file permissions.
#[cfg(not(windows))]
mod dpapi {
    use anyhow::Result;

    pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
        Ok(data.to_vec())
    }

    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
        Ok(data.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_persists_and_tracks_registrations() {
        let dir = tempfile::tempdir().unwrap();
        let first = Identity::load_or_create(dir.path()).unwrap();
        let again = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(first.public_key_base64(), again.public_key_base64());
        assert_ne!(
            dpapi::protect(&[1, 2, 3]).unwrap().len(),
            0,
            "protected data is not empty"
        );

        assert_eq!(first.worker_id("http://a"), None);
        first.save_worker_id("http://a", "w_1").unwrap();
        assert_eq!(again.worker_id("http://a").as_deref(), Some("w_1"));
        assert_eq!(
            again.worker_id("http://b"),
            None,
            "a different coordinator needs its own registration"
        );
    }
}
