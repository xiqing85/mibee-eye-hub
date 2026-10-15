//! AES-256-GCM encryption/decryption for sensitive config fields.
//!
//! Uses `NVR_ENCRYPTION_KEY` environment variable for key derivation.
//! The env var value is hashed with SHA-256 to produce a 256-bit key.
//!
//! If `NVR_ENCRYPTION_KEY` is not set, [`get_encryption_key`] returns `None`.
//! Callers should handle this gracefully (e.g., store plaintext with a warning).
//!
//! # Storage format
//!
//! Encrypted output is nonce (12 bytes) || ciphertext (includes 16-byte GCM tag).
//! Store as hex-encoded string in SQLite for inspectability.

use anyhow::{Context, Result};
use rand::RngCore;
use sha2::{Digest, Sha256};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};

/// Environment variable that holds the encryption key seed.
const ENV_KEY_NAME: &str = "NVR_ENCRYPTION_KEY";

/// Length of AES-256-GCM nonce in bytes (12 bytes / 96 bits — recommended for GCM).
const NONCE_LEN: usize = 12;

/// Derive a 256-bit AES key from the `NVR_ENCRYPTION_KEY` env var using SHA-256.
///
/// Returns `None` if the env var is not set.
pub fn get_encryption_key() -> Option<[u8; 32]> {
    let env_val = std::env::var(ENV_KEY_NAME).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(env_val.as_bytes());
    let result = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&result);
    Some(key)
}

/// Encrypt a plaintext string using AES-256-GCM.
///
/// Returns `nonce (12 bytes) || ciphertext (with appended 16-byte GCM tag)`.
///
/// Each call generates a fresh random nonce, so the same plaintext produces
/// different ciphertexts (semantic security).
pub fn encrypt_field(plaintext: &str, key: &[u8]) -> Result<Vec<u8>> {
    anyhow::ensure!(key.len() == 32, "AES-256 key must be exactly 32 bytes");

    // Generate random 12-byte nonce
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let key = aes_gcm::Key::<Aes256Gcm>::from_slice(key);
    let cipher = Aes256Gcm::new(key);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| anyhow::anyhow!("AES-256-GCM encryption failed: {:?}", e))?;
    // Prepend nonce to ciphertext for storage
    let mut result = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

/// Decrypt a ciphertext produced by [`encrypt_field`].
///
/// Expects the first 12 bytes to be the nonce, followed by the AES-256-GCM
/// ciphertext (including the 16-byte authentication tag).
pub fn decrypt_field(ciphertext: &[u8], key: &[u8]) -> Result<String> {
    anyhow::ensure!(key.len() == 32, "AES-256 key must be exactly 32 bytes");
    anyhow::ensure!(
        ciphertext.len() >= NONCE_LEN + 16,
        "ciphertext too short: {} bytes (need at least {} for nonce + tag)",
        ciphertext.len(),
        NONCE_LEN + 16,
    );

    let (nonce_bytes, encrypted) = ciphertext.split_at(NONCE_LEN);
    let nonce = Nonce::from_slice(nonce_bytes);

    let key = aes_gcm::Key::<Aes256Gcm>::from_slice(key);
    let cipher = Aes256Gcm::new(key);

    let plaintext = cipher
        .decrypt(nonce, encrypted)
        .map_err(|e| anyhow::anyhow!("AES-256-GCM decryption failed: {:?}", e))?;
    String::from_utf8(plaintext).context("decrypted data is not valid UTF-8")
}

/// Hex-encode bytes into a lowercase hex string.
///
/// This is the storage format for encrypted fields in SQLite.
pub fn hex_encode(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 2);
    for byte in data {
        use std::fmt::Write;
        // unwrap is safe: writing to String never fails
        write!(s, "{byte:02x}").unwrap();
    }
    s
}

/// Decode a hex string back into bytes.
///
/// Returns an error if the input is not valid lowercase hex (odd length
/// or invalid characters).
pub fn hex_decode(hex: &str) -> Result<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        anyhow::bail!("hex string has odd length: {}", hex.len());
    }
    // The slice indices below are byte offsets, so a multi-byte character
    // would land mid-codepoint and panic on the `&str` boundary check —
    // e.g. `"aée"` (4 bytes) fails at `&hex[2..4]`. Reject non-ASCII up front
    // so the documented "returns an error" contract actually holds.
    if !hex.is_ascii() {
        anyhow::bail!("hex string contains non-ASCII characters");
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .with_context(|| format!("invalid hex characters at position {i}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// A deterministic 32-byte test key derived from a known seed.
    fn test_key() -> [u8; 32] {
        let mut key = [0u8; 32];
        let hash = Sha256::digest(b"test-key-for-unit-tests");
        key.copy_from_slice(&hash);
        key
    }

    /// Serialises every test in this module.
    ///
    /// The process environment is global and `std::env::set_var` /
    /// `remove_var` are unsynchronised. Left to run in parallel, one test's
    /// `remove_env_var()` lands in the middle of another's
    /// `get_encryption_key()` — which is exactly what the `no-ai` CI leg
    /// caught: `test_different_env_var_produces_different_key` unwrapped
    /// `None` because a concurrent test had removed the variable out from
    /// under it. `get_encryption_key` returns `Option` precisely because the
    /// variable may be absent, so the failure surfaced as a bare `unwrap` on
    /// `None` with no hint at the real cause.
    ///
    /// The guard is always cleaned up, including on panic, so a failing test
    /// cannot leave the variable set for the next one.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take the module lock, set the variable, and return a guard that
    /// restores the previous value on drop.
    fn setup_env_var(key: &str) -> EnvGuard {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var(ENV_KEY_NAME).ok();
        // SAFETY: single-threaded within this test binary thanks to ENV_LOCK,
        // and no other thread reads the environment through this path.
        unsafe { std::env::set_var(ENV_KEY_NAME, key) };
        EnvGuard {
            _lock: lock,
            previous,
        }
    }

    /// Restores the environment on drop, panic or not.
    struct EnvGuard {
        /// Held for the lifetime of the guard: serialises the module.
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<String>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: the lock is still held here (fields drop after `drop`).
            unsafe {
                match &self.previous {
                    Some(v) => std::env::set_var(ENV_KEY_NAME, v),
                    None => std::env::remove_var(ENV_KEY_NAME),
                }
            }
        }
    }
    // ------------------------------------------------------------------
    // Key derivation
    // ------------------------------------------------------------------

    #[test]
    fn test_get_encryption_key_returns_none_when_unset() {
        // Take the module lock and make sure the variable is genuinely absent,
        // rather than relying on whatever a concurrently-running test left
        // behind.
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: ENV_LOCK is held, and it is the only writer of this
        // variable in this test binary.
        unsafe { std::env::remove_var(ENV_KEY_NAME) };
        assert!(get_encryption_key().is_none());
        drop(lock);
    }

    #[test]
    fn test_get_encryption_key_returns_32_bytes_when_set() {
        let _env = setup_env_var("test-encryption-key-12345");
        let key = get_encryption_key();
        assert!(key.is_some());
        assert_eq!(key.unwrap().len(), 32);
        drop(_env);
    }

    #[test]
    fn test_same_env_var_produces_same_key() {
        let _env = setup_env_var("my-secret-key");
        let k1 = get_encryption_key().unwrap();
        let k2 = get_encryption_key().unwrap();
        assert_eq!(k1, k2);
        drop(_env);
    }

    #[test]
    fn test_different_env_var_produces_different_key() {
        let _env = setup_env_var("key-one");
        let k1 = get_encryption_key().unwrap();
        // Change the value in place rather than taking a second guard: the
        // guard holds the module lock, and shadowing it would drop the first
        // one mid-test and reopen the window the lock exists to close.
        // SAFETY: `_env` holds the module lock for this whole scope.
        unsafe { std::env::set_var(ENV_KEY_NAME, "key-two") };
        let k2 = get_encryption_key().unwrap();
        assert_ne!(k1, k2);
        drop(_env);
    }

    #[test]
    fn test_empty_env_var_still_produces_key() {
        let _env = setup_env_var("");
        let key = get_encryption_key();
        assert!(key.is_some());
        assert_eq!(key.unwrap().len(), 32);
        drop(_env);
    }

    // ------------------------------------------------------------------
    // Encrypt / Decrypt round-trip
    // ------------------------------------------------------------------

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = test_key();
        let plaintext = "rtsp://admin:password123@192.168.1.100:554/stream1";

        let encrypted = encrypt_field(plaintext, &key).unwrap();
        assert_ne!(encrypted, plaintext.as_bytes());

        let decrypted = decrypt_field(&encrypted, &key).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_encrypt_decrypt_empty_string() {
        let key = test_key();
        let encrypted = encrypt_field("", &key).unwrap();
        let decrypted = decrypt_field(&encrypted, &key).unwrap();
        assert_eq!(decrypted, "");
    }

    #[test]
    fn test_encrypt_decrypt_unicode() {
        let key = test_key();
        let plaintext = "密码: 你好世界! 🔐";
        let encrypted = encrypt_field(plaintext, &key).unwrap();
        let decrypted = decrypt_field(&encrypted, &key).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_encrypt_decrypt_long_string() {
        let key = test_key();
        let plaintext = "A".repeat(10_000);
        let encrypted = encrypt_field(&plaintext, &key).unwrap();
        let decrypted = decrypt_field(&encrypted, &key).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_encrypt_produces_different_ciphertexts() {
        let key = test_key();
        let plaintext = "same-password-every-time";

        let c1 = encrypt_field(plaintext, &key).unwrap();
        let c2 = encrypt_field(plaintext, &key).unwrap();

        // Different nonces should produce different ciphertexts
        assert_ne!(c1, c2);

        // Both should decrypt correctly
        assert_eq!(decrypt_field(&c1, &key).unwrap(), plaintext);
        assert_eq!(decrypt_field(&c2, &key).unwrap(), plaintext);
    }

    #[test]
    fn test_encrypt_output_starts_with_nonce() {
        let key = test_key();
        let encrypted = encrypt_field("hello", &key).unwrap();
        // First 12 bytes are the nonce
        assert!(encrypted.len() > NONCE_LEN);
        let (nonce, _rest) = encrypted.split_at(NONCE_LEN);
        assert_eq!(nonce.len(), NONCE_LEN);
    }

    // ------------------------------------------------------------------
    // Decryption error cases
    // ------------------------------------------------------------------

    #[test]
    fn test_decrypt_wrong_key() {
        let key1 = test_key();
        let mut key2 = test_key();
        key2[0] ^= 0x01; // Flip one bit

        let plaintext = "sensitive-data";
        let encrypted = encrypt_field(plaintext, &key1).unwrap();

        // Decryption with wrong key should fail
        assert!(decrypt_field(&encrypted, &key2).is_err());
    }

    #[test]
    fn test_decrypt_truncated_ciphertext() {
        let key = test_key();
        assert!(decrypt_field(b"", &key).is_err());
        assert!(decrypt_field(b"too-short", &key).is_err());
        // 12 bytes nonce but no ciphertext/tag
        assert!(decrypt_field(&[0u8; 12], &key).is_err());
    }

    #[test]
    fn test_decrypt_tampered_ciphertext() {
        let key = test_key();
        let plaintext = "important-data";
        let mut encrypted = encrypt_field(plaintext, &key).unwrap();

        // Tamper with the last byte (part of GCM tag)
        let last = encrypted.len() - 1;
        encrypted[last] ^= 0xFF;

        assert!(decrypt_field(&encrypted, &key).is_err());
    }

    #[test]
    fn test_decrypt_wrong_key_length() {
        assert!(decrypt_field(b"some-ciphertext", &[0u8; 16]).is_err());
    }

    #[test]
    fn test_encrypt_wrong_key_length() {
        assert!(encrypt_field("test", &[0u8; 16]).is_err());
    }

    // ------------------------------------------------------------------
    // Hex encoding
    // ------------------------------------------------------------------

    #[test]
    fn test_hex_encode_decode_roundtrip() {
        let data = b"hello world";
        let encoded = hex_encode(data);
        assert_eq!(encoded, "68656c6c6f20776f726c64");
        let decoded = hex_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_hex_encode_decode_empty() {
        assert_eq!(hex_encode(b""), "");
        let decoded = hex_decode("").unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_hex_decode_invalid() {
        assert!(hex_decode("xyz").is_err());
        assert!(hex_decode("0xzz").is_err());
    }

    #[test]
    fn test_hex_decode_odd_length() {
        assert!(hex_decode("abc").is_err());
    }

    #[test]
    fn test_hex_encode_all_bytes() {
        let data: Vec<u8> = (0..=255).collect();
        let encoded = hex_encode(&data);
        let decoded = hex_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    // ------------------------------------------------------------------
    // Full pipeline integration
    // ------------------------------------------------------------------

    #[test]
    fn test_full_pipeline_env_var_to_hex_storage() {
        let _env = setup_env_var("super-secret-nvr-key-2024");
        let key = get_encryption_key().unwrap();

        let plaintext = "api_key_12345_abcdef";
        let encrypted = encrypt_field(plaintext, &key).unwrap();
        let hex_stored = hex_encode(&encrypted);

        // Simulate reading from DB
        let encrypted_again = hex_decode(&hex_stored).unwrap();
        let decrypted = decrypt_field(&encrypted_again, &key).unwrap();

        assert_eq!(decrypted, plaintext);
        drop(_env);
    }

    #[test]
    fn test_full_pipeline_multiple_fields() {
        let _env = setup_env_var("nvr-master-key-42");
        let key = get_encryption_key().unwrap();

        let fields = [
            ("camera_password", "hunter2!"),
            ("api_key", "sk-live-abc123def456"),
            ("nvr_token", "eyJhbGciOiJIUzI1NiJ9.secret"),
        ];

        for (name, value) in &fields {
            let encrypted = encrypt_field(value, &key).unwrap();
            let hex_stored = hex_encode(&encrypted);
            let encrypted_back = hex_decode(&hex_stored).unwrap();
            let decrypted = decrypt_field(&encrypted_back, &key).unwrap();
            assert_eq!(decrypted, *value, "field '{name}' round-trip failed");
        }
        drop(_env);
    }
}
