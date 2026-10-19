//! Passphrase encryption for export bundles: Argon2id derives 64 bytes, the
//! first half keys AES-256-GCM and the second half keys an HMAC-SHA256 check
//! value that tells a wrong passphrase apart from a modified ciphertext.

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::CsError;

pub(crate) const SCHEME: &str = "argon2id-aes256gcm";
const KEY_CHECK_LABEL: &[u8] = b"codex-switch-export key check v1";

pub(crate) const DEFAULT_M_COST_KIB: u32 = 64 * 1024;
pub(crate) const DEFAULT_T_COST: u32 = 3;
pub(crate) const DEFAULT_P_COST: u32 = 1;

const MIN_M_COST_KIB: u32 = 8 * 1024;
// Upper bounds are checked before deriving and before the passphrase is
// known, so a hostile bundle can cost at most ~4x the default work.
const MAX_M_COST_KIB: u32 = 256 * 1024;
const MAX_T_COST: u32 = 6;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_CHECK_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KdfParams {
    pub m_cost_kib: u32,
    pub t_cost: u32,
    pub p_cost: u32,
    pub salt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Encryption {
    pub scheme: String,
    pub kdf: KdfParams,
    pub nonce: String,
    pub key_check: String,
}

/// Keys derived from one passphrase + salt.
struct DerivedKeys {
    okm: Zeroizing<[u8; 64]>,
}

impl DerivedKeys {
    fn enc_key(&self) -> &[u8] {
        &self.okm[..32]
    }
    fn check_key(&self) -> &[u8] {
        &self.okm[32..]
    }
    fn key_check(&self) -> [u8; KEY_CHECK_LEN] {
        let key = hmac::Key::new(hmac::HMAC_SHA256, self.check_key());
        let tag = hmac::sign(&key, KEY_CHECK_LABEL);
        let mut out = [0u8; KEY_CHECK_LEN];
        out.copy_from_slice(tag.as_ref());
        out
    }
}

fn derive(passphrase: &str, salt: &[u8], m: u32, t: u32, p: u32) -> Result<DerivedKeys> {
    let params = argon2::Params::new(m, t, p, Some(64))
        .map_err(|e| CsError::UnsupportedEncryption(format!("argon2 parameters: {e}")))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut okm = Zeroizing::new([0u8; 64]);
    argon
        .hash_password_into(passphrase.as_bytes(), salt, okm.as_mut())
        .map_err(|e| CsError::UnsupportedEncryption(format!("argon2: {e}")))?;
    Ok(DerivedKeys { okm })
}

fn random_bytes<const N: usize>(rng: &SystemRandom) -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    rng.fill(&mut buf)
        .map_err(|_| anyhow::anyhow!("system random number generator failed"))?;
    Ok(buf)
}

/// Fresh salt/nonce plus the derived key check. The caller serialises the
/// header (which includes this) as AAD and then calls [`seal`].
pub(crate) struct Sealer {
    keys: DerivedKeys,
    nonce: [u8; NONCE_LEN],
    pub encryption: Encryption,
}

pub(crate) fn new_sealer(passphrase: &str) -> Result<Sealer> {
    new_sealer_with_params(
        passphrase,
        DEFAULT_M_COST_KIB,
        DEFAULT_T_COST,
        DEFAULT_P_COST,
    )
}

pub(crate) fn new_sealer_with_params(passphrase: &str, m: u32, t: u32, p: u32) -> Result<Sealer> {
    let rng = SystemRandom::new();
    let salt: [u8; SALT_LEN] = random_bytes(&rng)?;
    let nonce: [u8; NONCE_LEN] = random_bytes(&rng)?;
    let keys = derive(passphrase, &salt, m, t, p)?;
    let encryption = Encryption {
        scheme: SCHEME.to_string(),
        kdf: KdfParams {
            m_cost_kib: m,
            t_cost: t,
            p_cost: p,
            salt: B64.encode(salt),
        },
        nonce: B64.encode(nonce),
        key_check: B64.encode(keys.key_check()),
    };
    Ok(Sealer {
        keys,
        nonce,
        encryption,
    })
}

impl Sealer {
    pub(crate) fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
        let key = LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, self.keys.enc_key())
                .map_err(|_| anyhow::anyhow!("invalid AES-256-GCM key"))?,
        );
        let mut buf = plaintext.to_vec();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(self.nonce),
            Aad::from(aad),
            &mut buf,
        )
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;
        Ok(buf)
    }
}

fn decode_fixed<const N: usize>(field: &str, value: &str) -> Result<[u8; N]> {
    let raw = B64
        .decode(value)
        .map_err(|_| CsError::UnsupportedEncryption(format!("{field} is not valid base64")))?;
    raw.try_into()
        .map_err(|_| CsError::UnsupportedEncryption(format!("{field} must be {N} bytes")).into())
}

/// Reject a crafted header before running an expensive KDF on it.
pub(crate) fn check_bounds(enc: &Encryption) -> Result<()> {
    if enc.scheme != SCHEME {
        return Err(CsError::UnsupportedEncryption(format!("scheme '{}'", enc.scheme)).into());
    }
    let k = &enc.kdf;
    if !(MIN_M_COST_KIB..=MAX_M_COST_KIB).contains(&k.m_cost_kib) {
        return Err(CsError::UnsupportedEncryption(format!(
            "m_cost_kib {} outside {MIN_M_COST_KIB}..={MAX_M_COST_KIB}",
            k.m_cost_kib
        ))
        .into());
    }
    if !(1..=MAX_T_COST).contains(&k.t_cost) {
        return Err(CsError::UnsupportedEncryption(format!(
            "t_cost {} outside 1..={MAX_T_COST}",
            k.t_cost
        ))
        .into());
    }
    if !(1..=4).contains(&k.p_cost) {
        return Err(
            CsError::UnsupportedEncryption(format!("p_cost {} outside 1..=4", k.p_cost)).into(),
        );
    }
    decode_fixed::<SALT_LEN>("salt", &k.salt)?;
    decode_fixed::<NONCE_LEN>("nonce", &enc.nonce)?;
    decode_fixed::<KEY_CHECK_LEN>("key_check", &enc.key_check)?;
    Ok(())
}

/// Derive, verify the key check (wrong passphrase) and then open the AEAD
/// (tampering). Bounds must already have been checked.
pub(crate) fn open(
    enc: &Encryption,
    passphrase: &str,
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Zeroizing<Vec<u8>>> {
    check_bounds(enc)?;
    let salt = decode_fixed::<SALT_LEN>("salt", &enc.kdf.salt)?;
    let nonce = decode_fixed::<NONCE_LEN>("nonce", &enc.nonce)?;
    let expected = decode_fixed::<KEY_CHECK_LEN>("key_check", &enc.key_check)?;
    let keys = derive(
        passphrase,
        &salt,
        enc.kdf.m_cost_kib,
        enc.kdf.t_cost,
        enc.kdf.p_cost,
    )?;
    let check = hmac::Key::new(hmac::HMAC_SHA256, keys.check_key());
    if hmac::verify(&check, KEY_CHECK_LABEL, &expected).is_err() {
        return Err(CsError::WrongPassphrase.into());
    }
    let key = LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, keys.enc_key())
            .map_err(|_| anyhow::anyhow!("invalid AES-256-GCM key"))?,
    );
    let mut buf = Zeroizing::new(ciphertext.to_vec());
    let len = key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut buf,
        )
        .map_err(|_| CsError::BundleTampered)?
        .len();
    buf.truncate(len);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: u32 = MIN_M_COST_KIB;

    #[test]
    fn round_trip_and_distinct_errors() {
        let sealer = new_sealer_with_params("pw", M, 1, 1).unwrap();
        let ct = sealer.seal(b"aad", b"secret payload").unwrap();
        let enc = sealer.encryption.clone();
        assert_eq!(
            &open(&enc, "pw", b"aad", &ct).unwrap()[..],
            b"secret payload"
        );

        let err = open(&enc, "nope", b"aad", &ct).unwrap_err();
        assert!(matches!(err.downcast_ref(), Some(CsError::WrongPassphrase)));

        let mut bad = ct.clone();
        bad[0] ^= 1;
        let err = open(&enc, "pw", b"aad", &bad).unwrap_err();
        assert!(matches!(err.downcast_ref(), Some(CsError::BundleTampered)));

        let err = open(&enc, "pw", b"other aad", &ct).unwrap_err();
        assert!(matches!(err.downcast_ref(), Some(CsError::BundleTampered)));
    }

    #[test]
    fn bounds_rejected_before_derivation() {
        let mut enc = new_sealer_with_params("pw", M, 1, 1).unwrap().encryption;
        enc.kdf.m_cost_kib = 4 * 1024 * 1024;
        let started = std::time::Instant::now();
        let err = open(&enc, "pw", b"", b"").unwrap_err();
        assert!(matches!(
            err.downcast_ref(),
            Some(CsError::UnsupportedEncryption(_))
        ));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }
}
