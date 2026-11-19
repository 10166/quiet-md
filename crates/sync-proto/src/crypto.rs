use anyhow::{Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::entity::{sha256_hex, EntityKind};

const KEY_CHECK_PLAINTEXT: &[u8] = b"quiet-md-e2ee-v1";
pub const TOMBSTONE_PLAINTEXT: &[u8] = b"quiet-md-tombstone-v1";
const RECOVERY_PREFIX: &str = "QMD1";
const B32_ALPHA: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    pub alg: String,
    pub mem_kib: u32,
    pub time_cost: u32,
    pub parallelism: u32,
    pub salt_b64: String,
}

impl KdfParams {
    pub fn default_argon2id() -> Self {
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);
        Self {
            alg: "argon2id".into(),
            mem_kib: 65_536,
            time_cost: 3,
            parallelism: 1,
            salt_b64: base64::engine::general_purpose::STANDARD.encode(salt),
        }
    }

    #[cfg(test)]
    pub(crate) fn fast_for_test() -> Self {
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);
        Self { alg: "argon2id".into(), mem_kib: 4096, time_cost: 1, parallelism: 1, salt_b64: base64::engine::general_purpose::STANDARD.encode(salt) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct E2eeSetup {
    pub kdf: KdfParams,
    pub wrapped_vmk_b64: String,
    pub vmk_nonce_b64: String,
    pub key_check_b64: String,
    pub key_check_nonce_b64: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Envelope {
    Plain { data_b64: String },
    Encrypted { nonce_b64: String, ciphertext_b64: String },
}

impl Envelope {
    pub fn plain(bytes: &[u8]) -> Self {
        Self::Plain { data_b64: base64::engine::general_purpose::STANDARD.encode(bytes) }
    }

    pub fn decode_plain(&self) -> Result<Vec<u8>> {
        match self {
            Self::Plain { data_b64 } => base64::engine::general_purpose::STANDARD
                .decode(data_b64)
                .context("invalid plain envelope base64"),
            Self::Encrypted { .. } => anyhow::bail!("entity is end-to-end encrypted"),
        }
    }

    pub fn is_encrypted(&self) -> bool {
        matches!(self, Self::Encrypted { .. })
    }
}

#[derive(Clone)]
pub struct VaultKeys {
    vmk: [u8; 32],
}

impl Drop for VaultKeys {
    fn drop(&mut self) {
        for b in &mut self.vmk {
            // SAFETY: volatile write prevents dead-store elimination of key bytes.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

impl VaultKeys {
    pub fn generate() -> Self {
        let mut vmk = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut vmk);
        Self { vmk }
    }

    pub fn from_bytes(vmk: [u8; 32]) -> Self {
        Self { vmk }
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.vmk
    }

    pub fn to_recovery_key(&self) -> String {
        let mut raw = [0u8; 34];
        raw[..32].copy_from_slice(&self.vmk);
        let sum = Sha256::digest(self.vmk);
        raw[32] = sum[0];
        raw[33] = sum[1];
        let encoded = b32_encode(&raw);
        let groups: Vec<&str> = encoded.as_bytes().chunks(5).map(|c| std::str::from_utf8(c).unwrap()).collect();
        format!("{RECOVERY_PREFIX}-{}", groups.join("-"))
    }

    pub fn from_recovery_key(input: &str) -> Result<Self> {
        let cleaned: String = input.trim().chars().filter(|c| *c != '-' && !c.is_whitespace()).map(|c| c.to_ascii_uppercase()).collect();
        let body = cleaned.strip_prefix(RECOVERY_PREFIX).context("恢复密钥格式无效")?;
        let bytes = b32_decode(body).context("恢复密钥字符无效")?;
        anyhow::ensure!(bytes.len() == 34, "恢复密钥长度不正确");
        let mut vmk = [0u8; 32];
        vmk.copy_from_slice(&bytes[..32]);
        let sum = Sha256::digest(vmk);
        anyhow::ensure!(sum[0] == bytes[32] && sum[1] == bytes[33], "恢复密钥校验失败，请检查是否有字符输错");
        Ok(Self { vmk })
    }

    pub fn wrap_with_passphrase(&self, passphrase: &str) -> Result<E2eeSetup> {
        self.wrap_with_params(passphrase, KdfParams::default_argon2id())
    }

    pub fn wrap_with_params(&self, passphrase: &str, kdf: KdfParams) -> Result<E2eeSetup> {
        anyhow::ensure!(!passphrase.is_empty(), "同步口令不能为空");
        let pwk = derive_pwk(passphrase, &kdf)?;
        let cipher = XChaCha20Poly1305::new((&pwk).into());
        let mut vmk_nonce = [0u8; 24];
        rand::thread_rng().fill_bytes(&mut vmk_nonce);
        let wrapped = cipher
            .encrypt(XNonce::from_slice(&vmk_nonce), Payload { msg: &self.vmk, aad: b"quiet-md/vmk" })
            .map_err(|_| anyhow::anyhow!("wrap VMK failed"))?;

        let check_key = self.derive_subkey(b"quiet-md/v1/key-check");
        let check_cipher = XChaCha20Poly1305::new((&check_key).into());
        let mut check_nonce = [0u8; 24];
        rand::thread_rng().fill_bytes(&mut check_nonce);
        let key_check = check_cipher
            .encrypt(XNonce::from_slice(&check_nonce), Payload { msg: KEY_CHECK_PLAINTEXT, aad: b"quiet-md/check" })
            .map_err(|_| anyhow::anyhow!("encrypt key check failed"))?;

        let b64 = base64::engine::general_purpose::STANDARD;
        Ok(E2eeSetup {
            kdf,
            wrapped_vmk_b64: b64.encode(wrapped),
            vmk_nonce_b64: b64.encode(vmk_nonce),
            key_check_b64: b64.encode(key_check),
            key_check_nonce_b64: b64.encode(check_nonce),
        })
    }

    pub fn unwrap_with_passphrase(passphrase: &str, setup: &E2eeSetup) -> Result<Self> {
        let pwk = derive_pwk(passphrase, &setup.kdf)?;
        let b64 = base64::engine::general_purpose::STANDARD;
        let nonce = b64.decode(&setup.vmk_nonce_b64).context("invalid vmk nonce")?;
        anyhow::ensure!(nonce.len() == 24, "invalid vmk nonce length");
        let wrapped = b64.decode(&setup.wrapped_vmk_b64).context("invalid wrapped vmk")?;
        let cipher = XChaCha20Poly1305::new((&pwk).into());
        let raw = cipher
            .decrypt(XNonce::from_slice(&nonce), Payload { msg: &wrapped, aad: b"quiet-md/vmk" })
            .map_err(|_| anyhow::anyhow!("同步口令错误"))?;
        anyhow::ensure!(raw.len() == 32, "invalid VMK length");
        let mut vmk = [0u8; 32];
        vmk.copy_from_slice(&raw);
        let keys = Self { vmk };
        keys.verify_setup(setup)?;
        Ok(keys)
    }

    pub fn verify_setup(&self, setup: &E2eeSetup) -> Result<()> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let check_nonce = b64.decode(&setup.key_check_nonce_b64).context("invalid check nonce")?;
        anyhow::ensure!(check_nonce.len() == 24, "invalid check nonce length");
        let check_ct = b64.decode(&setup.key_check_b64).context("invalid check ciphertext")?;
        let check_key = self.derive_subkey(b"quiet-md/v1/key-check");
        let cipher = XChaCha20Poly1305::new((&check_key).into());
        let pt = cipher
            .decrypt(XNonce::from_slice(&check_nonce), Payload { msg: &check_ct, aad: b"quiet-md/check" })
            .map_err(|_| anyhow::anyhow!("恢复密钥与该加密文档库不匹配"))?;
        anyhow::ensure!(pt == KEY_CHECK_PLAINTEXT, "恢复密钥校验失败");
        Ok(())
    }

    fn derive_subkey(&self, info: &[u8]) -> [u8; 32] {
        let hk = Hkdf::<Sha256>::new(Some(b"quiet-md-e2ee-salt-v1"), &self.vmk);
        let mut okm = [0u8; 32];
        hk.expand(info, &mut okm).expect("32 bytes is valid HKDF output length");
        okm
    }

    pub fn asset_entity_id(&self, rel_path: &str) -> String {
        let key = self.derive_subkey(b"quiet-md/v1/asset-id");
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&key).expect("HMAC accepts any key size");
        mac.update(rel_path.as_bytes());
        let tag = mac.finalize().into_bytes();
        let mut hex = String::with_capacity(32);
        for b in &tag[..16] {
            use std::fmt::Write;
            let _ = write!(&mut hex, "{b:02x}");
        }
        format!("asset:{hex}")
    }

    pub fn encrypt_entity(&self, vault_id: &str, entity_id: &str, kind: EntityKind, version: u64, plaintext: &[u8]) -> Result<Envelope> {
        self.encrypt_with_mode(vault_id, entity_id, kind, version, false, plaintext)
    }

    pub fn decrypt_entity(&self, vault_id: &str, entity_id: &str, kind: EntityKind, version: u64, env: &Envelope) -> Result<Vec<u8>> {
        self.decrypt_with_mode(vault_id, entity_id, kind, version, false, env)
    }

    pub fn encrypt_tombstone(&self, vault_id: &str, entity_id: &str, kind: EntityKind, version: u64) -> Result<Envelope> {
        self.encrypt_with_mode(vault_id, entity_id, kind, version, true, TOMBSTONE_PLAINTEXT)
    }

    pub fn verify_tombstone(&self, vault_id: &str, entity_id: &str, kind: EntityKind, version: u64, env: &Envelope) -> Result<()> {
        let pt = self.decrypt_with_mode(vault_id, entity_id, kind, version, true, env)?;
        anyhow::ensure!(pt == TOMBSTONE_PLAINTEXT, "invalid encrypted tombstone payload");
        Ok(())
    }

    fn encrypt_with_mode(&self, vault_id: &str, entity_id: &str, kind: EntityKind, version: u64, deleted: bool, plaintext: &[u8]) -> Result<Envelope> {
        let key = self.derive_subkey(b"quiet-md/v1/entity");
        let cipher = XChaCha20Poly1305::new((&key).into());
        let mut nonce = [0u8; 24];
        rand::thread_rng().fill_bytes(&mut nonce);
        let aad = entity_aad(vault_id, entity_id, kind, version, deleted);
        let ct = cipher
            .encrypt(XNonce::from_slice(&nonce), Payload { msg: plaintext, aad: aad.as_bytes() })
            .map_err(|_| anyhow::anyhow!("entity encryption failed"))?;
        let b64 = base64::engine::general_purpose::STANDARD;
        Ok(Envelope::Encrypted { nonce_b64: b64.encode(nonce), ciphertext_b64: b64.encode(ct) })
    }

    fn decrypt_with_mode(&self, vault_id: &str, entity_id: &str, kind: EntityKind, version: u64, deleted: bool, env: &Envelope) -> Result<Vec<u8>> {
        match env {
            Envelope::Plain { .. } => anyhow::bail!("encrypted vault rejected plaintext envelope"),
            Envelope::Encrypted { nonce_b64, ciphertext_b64 } => {
                let b64 = base64::engine::general_purpose::STANDARD;
                let nonce = b64.decode(nonce_b64).context("invalid entity nonce")?;
                anyhow::ensure!(nonce.len() == 24, "invalid entity nonce length");
                let ct = b64.decode(ciphertext_b64).context("invalid entity ciphertext")?;
                let key = self.derive_subkey(b"quiet-md/v1/entity");
                let cipher = XChaCha20Poly1305::new((&key).into());
                let aad = entity_aad(vault_id, entity_id, kind, version, deleted);
                cipher
                    .decrypt(XNonce::from_slice(&nonce), Payload { msg: &ct, aad: aad.as_bytes() })
                    .map_err(|_| anyhow::anyhow!("entity decryption failed (tampered or wrong key)"))
            }
        }
    }

    /// Convergent per-vault blob encryption so identical attachments in the same vault deduplicate on server.
    pub fn encrypt_blob(&self, vault_id: &str, plaintext: &[u8]) -> Result<(String, Vec<u8>)> {
        let nonce_key = self.derive_subkey(b"quiet-md/v1/blob-nonce");
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&nonce_key).expect("HMAC accepts any key size");
        mac.update(vault_id.as_bytes());
        mac.update(b"|");
        mac.update(&Sha256::digest(plaintext));
        let tag = mac.finalize().into_bytes();
        let mut nonce = [0u8; 24];
        nonce.copy_from_slice(&tag[..24]);

        let key = self.derive_subkey(b"quiet-md/v1/blob");
        let cipher = XChaCha20Poly1305::new((&key).into());
        let aad = format!("{vault_id}|blob");
        let ct = cipher
            .encrypt(XNonce::from_slice(&nonce), Payload { msg: plaintext, aad: aad.as_bytes() })
            .map_err(|_| anyhow::anyhow!("blob encryption failed"))?;
        let mut out = Vec::with_capacity(24 + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        let hash = sha256_hex(&out);
        Ok((hash, out))
    }

    pub fn decrypt_blob(&self, vault_id: &str, blob_bytes: &[u8]) -> Result<Vec<u8>> {
        anyhow::ensure!(blob_bytes.len() > 24, "encrypted blob too short");
        let (nonce, ct) = blob_bytes.split_at(24);
        let key = self.derive_subkey(b"quiet-md/v1/blob");
        let cipher = XChaCha20Poly1305::new((&key).into());
        let aad = format!("{vault_id}|blob");
        cipher
            .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad: aad.as_bytes() })
            .map_err(|_| anyhow::anyhow!("blob decryption failed"))
    }
}

fn entity_aad(vault_id: &str, entity_id: &str, kind: EntityKind, version: u64, deleted: bool) -> String {
    let op = if deleted { "del" } else { "put" };
    format!("{vault_id}|{entity_id}|{}|{version}|{op}", kind.as_str())
}

fn derive_pwk(passphrase: &str, kdf: &KdfParams) -> Result<[u8; 32]> {
    anyhow::ensure!(kdf.alg == "argon2id", "unsupported KDF: {}", kdf.alg);
    anyhow::ensure!((4_096..=262_144).contains(&kdf.mem_kib), "Argon2 memory parameter out of safe bounds");
    anyhow::ensure!((1..=10).contains(&kdf.time_cost), "Argon2 time parameter out of safe bounds");
    anyhow::ensure!((1..=4).contains(&kdf.parallelism), "Argon2 parallelism out of safe bounds");
    let salt = base64::engine::general_purpose::STANDARD.decode(&kdf.salt_b64).context("invalid KDF salt")?;
    anyhow::ensure!((8..=64).contains(&salt.len()), "KDF salt length out of bounds");
    let params = Params::new(kdf.mem_kib, kdf.time_cost, kdf.parallelism, Some(32))
        .map_err(|e| anyhow::anyhow!("invalid Argon2 params: {e}"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; 32];
    argon
        .hash_password_into(passphrase.as_bytes(), &salt, &mut out)
        .map_err(|e| anyhow::anyhow!("Argon2 derivation failed: {e}"))?;
    Ok(out)
}

fn b32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() * 8).div_ceil(5));
    let mut buf: u16 = 0;
    let mut bits: u8 = 0;
    for &b in data {
        buf = (buf << 8) | b as u16;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32_ALPHA[((buf >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32_ALPHA[((buf << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

fn b32_decode(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity((s.len() * 5) / 8);
    let mut buf: u32 = 0;
    let mut bits: u8 = 0;
    for ch in s.bytes() {
        let val = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'2'..=b'7' => 26 + (ch - b'2'),
            _ => anyhow::bail!("invalid base32 char"),
        };
        buf = (buf << 5) | val as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    anyhow::ensure!(bits < 5 && (buf & ((1u32 << bits) - 1)) == 0, "恢复密钥尾部校验位无效");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passphrase_wrap_unwrap_and_recovery_key() {
        let keys = VaultKeys::generate();
        let setup = keys.wrap_with_params("my-secret-pass", KdfParams::fast_for_test()).unwrap();
        let unwrapped = VaultKeys::unwrap_with_passphrase("my-secret-pass", &setup).unwrap();
        assert_eq!(unwrapped.as_bytes(), keys.as_bytes());
        assert!(VaultKeys::unwrap_with_passphrase("wrong-pass", &setup).is_err());

        let rec = keys.to_recovery_key();
        assert!(rec.starts_with("QMD1-"));
        let recovered = VaultKeys::from_recovery_key(&rec.to_lowercase().replace('-', " ")).unwrap();
        assert_eq!(recovered.as_bytes(), keys.as_bytes());
        assert!(recovered.verify_setup(&setup).is_ok());

        // Flip either last char or an interior char: every single-char typo must fail checksum/padding.
        let mut chars: Vec<char> = rec.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
        let bad_last: String = chars.iter().collect();
        assert!(VaultKeys::from_recovery_key(&bad_last).is_err());

        let mut chars_mid: Vec<char> = rec.chars().collect();
        let mid = 8;
        chars_mid[mid] = if chars_mid[mid] == 'A' { 'B' } else { 'A' };
        let bad_mid: String = chars_mid.into_iter().collect();
        assert!(VaultKeys::from_recovery_key(&bad_mid).is_err());
    }

    #[test]
    fn entity_encryption_binds_aad_and_prevents_swap() {
        let keys = VaultKeys::generate();
        let env = keys.encrypt_entity("vault-1", "doc-a", EntityKind::Document, 3, b"hello world").unwrap();
        assert!(env.is_encrypted());
        assert_eq!(keys.decrypt_entity("vault-1", "doc-a", EntityKind::Document, 3, &env).unwrap(), b"hello world");
        assert!(keys.decrypt_entity("vault-1", "doc-b", EntityKind::Document, 3, &env).is_err());
        assert!(keys.decrypt_entity("vault-1", "doc-a", EntityKind::Document, 4, &env).is_err());
        assert!(keys.decrypt_entity("vault-2", "doc-a", EntityKind::Document, 3, &env).is_err());
        assert!(keys.decrypt_entity("vault-1", "doc-a", EntityKind::Folder, 3, &env).is_err());
    }

    #[test]
    fn convergent_blob_encryption_dedups_within_vault_only() {
        let keys = VaultKeys::generate();
        let (h1, c1) = keys.encrypt_blob("vault-1", b"png-bytes").unwrap();
        let (h2, c2) = keys.encrypt_blob("vault-1", b"png-bytes").unwrap();
        let (h3, c3) = keys.encrypt_blob("vault-2", b"png-bytes").unwrap();
        assert_eq!(h1, h2);
        assert_eq!(c1, c2);
        assert_ne!(h1, h3);
        assert_ne!(c1, c3);
        assert_eq!(keys.decrypt_blob("vault-1", &c1).unwrap(), b"png-bytes");
        assert!(keys.decrypt_blob("vault-2", &c1).is_err());
    }

    #[test]
    fn strict_e2ee_rejects_plaintext_binds_tombstones_hides_asset_names_and_bounds_kdf() {
        let keys = VaultKeys::generate();
        let plain = Envelope::plain(b"forged");
        assert!(keys.decrypt_entity("v1", "d1", EntityKind::Document, 1, &plain).is_err());
        assert!(keys.verify_tombstone("v1", "d1", EntityKind::Document, 1, &plain).is_err());

        let tomb = keys.encrypt_tombstone("v1", "d1", EntityKind::Document, 2).unwrap();
        assert!(keys.verify_tombstone("v1", "d1", EntityKind::Document, 2, &tomb).is_ok());
        assert!(keys.verify_tombstone("v1", "d2", EntityKind::Document, 2, &tomb).is_err());
        assert!(keys.verify_tombstone("v1", "d1", EntityKind::Folder, 2, &tomb).is_err());
        assert!(keys.verify_tombstone("v1", "d1", EntityKind::Document, 3, &tomb).is_err());
        assert!(keys.decrypt_entity("v1", "d1", EntityKind::Document, 2, &tomb).is_err());

        let put_env = keys.encrypt_entity("v1", "d1", EntityKind::Document, 2, TOMBSTONE_PLAINTEXT).unwrap();
        assert!(keys.verify_tombstone("v1", "d1", EntityKind::Document, 2, &put_env).is_err());

        let aid1 = keys.asset_entity_id(".assets/files/123/secret-passport.pdf");
        let aid2 = keys.asset_entity_id(".assets/files/123/secret-passport.pdf");
        assert_eq!(aid1, aid2);
        assert!(aid1.starts_with("asset:"));
        assert!(!aid1.contains("secret-passport"));

        let mut extreme = KdfParams::fast_for_test();
        extreme.mem_kib = 2_000_000;
        assert!(VaultKeys::unwrap_with_passphrase("pass", &E2eeSetup {
            kdf: extreme,
            wrapped_vmk_b64: String::new(),
            vmk_nonce_b64: String::new(),
            key_check_b64: String::new(),
            key_check_nonce_b64: String::new(),
        }).is_err());
    }
}
