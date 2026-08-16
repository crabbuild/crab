//! Encrypted on-disk token storage.
//!
//! Tokens are stored as JSON files encrypted with ChaCha20-Poly1305 using a
//! machine-local key. Each provider gets its own file:
//! `{cache_dir}/{provider_name}.json.enc`.
//!
//! File-level locking (`flock` on Unix) prevents concurrent crab processes
//! from corrupting the cache during login/logout/refresh.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::error::{AuthError, Result};

/// Size of the ChaCha20-Poly1305 nonce in bytes.
const NONCE_SIZE: usize = 12;

/// Encrypted on-disk token storage.
///
/// Tokens are stored as JSON files encrypted with a machine-local key.
/// Each provider gets its own file: `{cache_dir}/{provider_name}.json.enc`.
///
/// File-level locking (`flock` on Unix) prevents concurrent crab
/// processes from corrupting the cache during login/logout/refresh.
pub struct TokenCache {
    cache_dir: PathBuf,
    /// Encryption key derived from machine identity.
    key: [u8; 32],
}

/// Cached token set for a single provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedTokens {
    /// The OIDC ID token (JWT).
    pub id_token: String,
    /// OAuth2 access token used for managed service API authorization.
    #[serde(default)]
    pub access_token: Option<String>,
    /// The OIDC refresh token, if the IdP issued one.
    pub refresh_token: Option<String>,
    /// Identity claims extracted from the ID token.
    pub identity: TokenIdentity,
    /// Unix timestamp when the ID token was issued.
    pub issued_at: u64,
    /// Unix timestamp at which the access token expires, when known.
    #[serde(default)]
    pub expires_at: Option<u64>,
}

/// Identity claims extracted from a JWT ID token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenIdentity {
    /// The `sub` (subject) claim — unique user identifier at the IdP.
    pub subject: String,
    /// The `email` claim, if present.
    pub email: Option<String>,
    /// The `name` claim, if present.
    pub name: Option<String>,
}

/// Expands the configured token-cache path.
#[must_use]
pub fn expand_token_cache_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"))
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

impl TokenCache {
    /// Create or open the token cache at the given directory.
    ///
    /// Creates the directory if it doesn't exist. Loads (or generates) the
    /// machine-local encryption key from macOS Keychain or a fallback file
    /// at `~/.config/crab/.token-key`.
    pub fn new(cache_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&cache_dir)?;
        let key = load_or_create_key()?;
        Ok(Self { cache_dir, key })
    }

    /// Store tokens for the given provider. Encrypts and writes atomically
    /// via tempfile + rename.
    pub fn store(&self, provider: &str, id_token: &str, refresh_token: Option<&str>) -> Result<()> {
        self.store_tokens(provider, id_token, None, refresh_token, None)
    }

    /// Stores an OIDC token set including its service API access-token lifetime.
    pub fn store_oidc_tokens(
        &self,
        provider: &str,
        id_token: &str,
        access_token: &str,
        refresh_token: Option<&str>,
        expires_in_seconds: u64,
    ) -> Result<()> {
        let issued_at = unix_now();
        self.store_tokens(
            provider,
            id_token,
            Some(access_token),
            refresh_token,
            Some(issued_at.saturating_add(expires_in_seconds)),
        )
    }

    fn store_tokens(
        &self,
        provider: &str,
        id_token: &str,
        access_token: Option<&str>,
        refresh_token: Option<&str>,
        expires_at: Option<u64>,
    ) -> Result<()> {
        let identity = Self::parse_identity(id_token)?;
        let issued_at = unix_now();

        let cached = CachedTokens {
            id_token: id_token.to_owned(),
            access_token: access_token.map(str::to_owned),
            refresh_token: refresh_token.map(str::to_owned),
            identity,
            issued_at,
            expires_at,
        };

        let plaintext =
            serde_json::to_vec(&cached).map_err(|source| AuthError::SerializeTokens { source })?;

        let ciphertext = self.encrypt(&plaintext)?;
        let path = self.token_path(provider);

        // Atomic write: tempfile in the same directory, then rename.
        let dir = self.cache_dir.clone();
        let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
        tmp.write_all(&ciphertext)?;
        tmp.flush()?;

        // flock the target file's parent dir to serialize concurrent writes.
        let _lock = flock_dir(&dir)?;
        tmp.persist(&path).map_err(|e| AuthError::Io(e.error))?;
        Ok(())
    }

    /// Load tokens for the given provider. Returns `None` if not cached.
    ///
    /// Decrypts and deserializes the token file. Returns `None` if the file
    /// does not exist. Returns an error on decryption or parse failure.
    pub fn load(&self, provider: &str) -> Result<Option<CachedTokens>> {
        let path = self.token_path(provider);
        if !path.exists() {
            return Ok(None);
        }

        let _lock = flock_dir(&self.cache_dir)?;
        let ciphertext = fs::read(&path)?;
        let plaintext = self.decrypt(&ciphertext)?;

        let cached: CachedTokens = serde_json::from_slice(&plaintext)
            .map_err(|source| AuthError::ParseCachedTokens { source })?;
        Ok(Some(cached))
    }

    /// Load tokens from the first existing provider key in `providers`.
    ///
    /// This is useful for callers that intentionally support multiple
    /// provider-specific cache names.
    pub fn load_any(&self, providers: &[&str]) -> Result<Option<CachedTokens>> {
        for provider in providers {
            if let Some(tokens) = self.load(provider)? {
                return Ok(Some(tokens));
            }
        }
        Ok(None)
    }

    /// Delete tokens for the given provider.
    pub fn delete(&self, provider: &str) -> Result<()> {
        let path = self.token_path(provider);
        let _lock = flock_dir(&self.cache_dir)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Delete tokens for all given provider keys.
    pub fn delete_all_names(&self, providers: &[&str]) -> Result<()> {
        for provider in providers {
            self.delete(provider)?;
        }
        Ok(())
    }

    /// Delete tokens for all providers.
    pub fn delete_all(&self) -> Result<()> {
        let _lock = flock_dir(&self.cache_dir)?;
        let entries = match fs::read_dir(&self.cache_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };

        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".json.enc") {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }

    /// Extract identity claims from a JWT ID token.
    ///
    /// Splits the JWT on `.`, base64-decodes the second part (payload), and
    /// extracts `sub`, `email`, `name` fields. No signature verification —
    /// the IdP already validated it.
    pub fn parse_identity(id_token: &str) -> Result<TokenIdentity> {
        let parts: Vec<&str> = id_token.splitn(3, '.').collect();
        if parts.len() < 2 {
            return Err(AuthError::InvalidJwt(
                "expected at least header.payload".into(),
            ));
        }

        let payload_bytes = URL_SAFE_NO_PAD
            .decode(parts[1])
            .map_err(|source| AuthError::JwtPayloadBase64 { source })?;

        let claims: serde_json::Value = serde_json::from_slice(&payload_bytes)
            .map_err(|source| AuthError::JwtPayloadJson { source })?;

        let subject = claims
            .get("sub")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AuthError::InvalidJwt("missing required 'sub' claim".into()))?
            .to_owned();

        let email = claims
            .get("email")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let name = claims
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_owned);

        Ok(TokenIdentity {
            subject,
            email,
            name,
        })
    }

    /// Path to the encrypted token file for a provider.
    fn token_path(&self, provider: &str) -> PathBuf {
        self.cache_dir.join(format!("{provider}.json.enc"))
    }

    /// Encrypt plaintext with ChaCha20-Poly1305. Prepends a random 12-byte nonce.
    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let key = Key::from_slice(&self.key);
        let cipher = ChaCha20Poly1305::new(key);

        let mut nonce_bytes = [0u8; NONCE_SIZE];
        rand::rng().fill(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, plaintext)
            .map_err(|e| AuthError::Crypto {
                operation: "encryption",
                reason: e.to_string(),
            })?;

        let mut out = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypt ciphertext. Expects the 12-byte nonce prepended to the data.
    fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < NONCE_SIZE {
            return Err(AuthError::Crypto {
                operation: "decryption",
                reason: "encrypted token file too short".into(),
            });
        }

        let (nonce_bytes, ciphertext) = data.split_at(NONCE_SIZE);
        let key = Key::from_slice(&self.key);
        let cipher = ChaCha20Poly1305::new(key);
        let nonce = Nonce::from_slice(nonce_bytes);

        cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| AuthError::Crypto {
                operation: "decryption",
                reason: e.to_string(),
            })
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Machine-local encryption key management
// ---------------------------------------------------------------------------

/// Load or create the machine-local encryption key.
///
/// On macOS, tries the Keychain via the `security` CLI first. If that fails
/// (or on other platforms), falls back to a file at
/// `~/.config/crab/.token-key` with `0600` permissions.
///
/// Set `CRAB_NO_KEYCHAIN=1` to skip Keychain entirely (useful for testing,
/// SSH sessions, or environments where the Keychain triggers GUI dialogs).
fn load_or_create_key() -> Result<[u8; 32]> {
    #[cfg(target_os = "macos")]
    {
        let skip_keychain = std::env::var("CRAB_NO_KEYCHAIN")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        if !skip_keychain {
            match keychain_load_key() {
                Ok(key) => return Ok(key),
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        "macOS Keychain unavailable, falling back to file-based key"
                    );
                }
            }
        } else {
            tracing::debug!("CRAB_NO_KEYCHAIN set, using file-based key");
        }
    }

    file_based_key()
}

/// Try to load the encryption key from macOS Keychain via the `security` CLI.
///
/// First checks that a usable keychain exists (via `security list-keychains`)
/// to avoid triggering a macOS GUI dialog when no keychain is available
/// (e.g., in SSH sessions, CI, or sandboxed environments).
#[cfg(target_os = "macos")]
fn keychain_load_key() -> Result<[u8; 32]> {
    use std::process::{Command, Stdio};

    // Pre-check: verify a login keychain is available. `list-keychains` never
    // triggers a GUI dialog. If no keychain is listed, skip entirely.
    let list_output = Command::new("security")
        .args(["list-keychains", "-d", "user"])
        .stderr(Stdio::null())
        .output()?;

    if !list_output.status.success() {
        return Err(AuthError::KeyStore(
            "security list-keychains failed — no usable keychain".into(),
        ));
    }

    let keychains = String::from_utf8_lossy(&list_output.stdout);
    if keychains.trim().is_empty() || !keychains.contains("login.keychain") {
        return Err(AuthError::KeyStore(
            "no login keychain found — skipping Keychain storage".into(),
        ));
    }

    let service = "crab-token-cache";
    let account = "encryption-key";

    // Try to read existing key.
    let output = Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .output()?;

    if output.status.success() {
        let hex_str = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        return hex_to_key(&hex_str);
    }

    // Key doesn't exist — generate and store.
    let mut key = [0u8; 32];
    rand::rng().fill(&mut key);
    let hex_str = key.iter().map(|b| format!("{b:02x}")).collect::<String>();

    let status = Command::new("security")
        .args([
            "add-generic-password",
            "-s",
            service,
            "-a",
            account,
            "-w",
            &hex_str,
            "-U", // update if exists
        ])
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .status()?;

    if !status.success() {
        return Err(AuthError::KeyStore(
            "failed to store encryption key in macOS Keychain".into(),
        ));
    }

    Ok(key)
}

/// Decode a 64-char hex string into a 32-byte key.
#[cfg(target_os = "macos")]
fn hex_to_key(hex: &str) -> Result<[u8; 32]> {
    if hex.len() != 64 {
        return Err(AuthError::KeyStore(format!(
            "keychain key has unexpected length {} (expected 64 hex chars)",
            hex.len()
        )));
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|e| AuthError::KeyStore(format!("keychain key contains invalid hex: {e}")))?;
    }
    Ok(key)
}

/// Load or create the encryption key from `~/.config/crab/.token-key`.
fn file_based_key() -> Result<[u8; 32]> {
    let key_dir = dirs_key_dir()?;
    fs::create_dir_all(&key_dir)?;
    let key_path = key_dir.join(".token-key");

    if key_path.exists() {
        let data = fs::read(&key_path)?;
        if data.len() != 32 {
            return Err(AuthError::KeyStore(format!(
                "token key file has unexpected size {} (expected 32 bytes)",
                data.len()
            )));
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&data);
        return Ok(key);
    }

    // Generate a new random key.
    let mut key = [0u8; 32];
    rand::rng().fill(&mut key);

    // Write with restrictive permissions.
    write_key_file(&key_path, &key)?;
    Ok(key)
}

/// Write the key file with 0600 permissions on Unix.
fn write_key_file(path: &Path, key: &[u8; 32]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(key)?;
        f.flush()?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        f.write_all(key)?;
        f.flush()?;
        Ok(())
    }
}

/// Resolve `~/.config/crab/` for the key file location.
fn dirs_key_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| AuthError::KeyStore("cannot determine home directory for token key".into()))?;
    Ok(PathBuf::from(home).join(".config").join("crab"))
}

// ---------------------------------------------------------------------------
// File-level locking
// ---------------------------------------------------------------------------

/// RAII guard for an `flock`-based directory lock.
///
/// On Unix, opens a `.lock` file in the directory and holds an exclusive
/// `flock` until dropped. On non-Unix, this is a no-op.
struct FlockGuard {
    #[cfg(unix)]
    _file: fs::File,
}

/// Acquire an exclusive `flock` on a `.lock` file inside `dir`.
fn flock_dir(dir: &Path) -> Result<FlockGuard> {
    #[cfg(unix)]
    {
        let lock_path = dir.join(".lock");
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;

        // SAFETY: `flock` is a POSIX API. We pass a valid fd obtained from
        // `File::open`. `LOCK_EX` blocks until the lock is acquired.
        let ret =
            unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) };
        if ret != 0 {
            return Err(std::io::Error::last_os_error().into());
        }

        Ok(FlockGuard { _file: file })
    }

    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(FlockGuard {})
    }
}

#[cfg(unix)]
impl Drop for FlockGuard {
    fn drop(&mut self) {
        // Unlock is automatic when the fd is closed, but be explicit.
        // Ignore errors on drop — nothing useful to do.
        unsafe {
            libc::flock(
                std::os::unix::io::AsRawFd::as_raw_fd(&self._file),
                libc::LOCK_UN,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal JWT with the given claims JSON as the payload.
    fn make_jwt(claims_json: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"RS256\"}");
        let payload = URL_SAFE_NO_PAD.encode(claims_json.as_bytes());
        format!("{header}.{payload}.fake-signature")
    }

    #[test]
    fn parse_identity_extracts_all_claims() {
        let jwt = make_jwt(r#"{"sub":"user-123","email":"alice@example.com","name":"Alice"}"#);
        let id = TokenCache::parse_identity(&jwt).unwrap();
        assert_eq!(id.subject, "user-123");
        assert_eq!(id.email.as_deref(), Some("alice@example.com"));
        assert_eq!(id.name.as_deref(), Some("Alice"));
    }

    #[test]
    fn parse_identity_missing_optional_claims() {
        let jwt = make_jwt(r#"{"sub":"user-456"}"#);
        let id = TokenCache::parse_identity(&jwt).unwrap();
        assert_eq!(id.subject, "user-456");
        assert!(id.email.is_none());
        assert!(id.name.is_none());
    }

    #[test]
    fn parse_identity_missing_sub_is_error() {
        let jwt = make_jwt(r#"{"email":"bob@example.com"}"#);
        assert!(TokenCache::parse_identity(&jwt).is_err());
    }

    #[test]
    fn parse_identity_malformed_jwt_is_error() {
        assert!(TokenCache::parse_identity("not-a-jwt").is_err());
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let cache = TokenCache {
            cache_dir: PathBuf::from("/tmp"),
            key: [42u8; 32],
        };
        let plaintext = b"hello, tokens!";
        let ciphertext = cache.encrypt(plaintext).unwrap();
        let decrypted = cache.decrypt(&ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn decrypt_wrong_key_fails() {
        let cache1 = TokenCache {
            cache_dir: PathBuf::from("/tmp"),
            key: [1u8; 32],
        };
        let cache2 = TokenCache {
            cache_dir: PathBuf::from("/tmp"),
            key: [2u8; 32],
        };
        let ciphertext = cache1.encrypt(b"secret").unwrap();
        assert!(cache2.decrypt(&ciphertext).is_err());
    }

    #[test]
    fn decrypt_too_short_fails() {
        let cache = TokenCache {
            cache_dir: PathBuf::from("/tmp"),
            key: [0u8; 32],
        };
        assert!(cache.decrypt(&[0u8; 5]).is_err());
    }

    #[test]
    fn store_load_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TokenCache {
            cache_dir: dir.path().to_path_buf(),
            key: [99u8; 32],
        };

        let jwt = make_jwt(r#"{"sub":"u1","email":"a@b.com","name":"A"}"#);

        // Store
        cache
            .store("test-provider", &jwt, Some("refresh-tok"))
            .unwrap();

        // Load
        let loaded = cache.load("test-provider").unwrap().unwrap();
        assert_eq!(loaded.id_token, jwt);
        assert_eq!(loaded.refresh_token.as_deref(), Some("refresh-tok"));
        assert_eq!(loaded.identity.subject, "u1");
        assert_eq!(loaded.identity.email.as_deref(), Some("a@b.com"));

        // Delete
        cache.delete("test-provider").unwrap();
        assert!(cache.load("test-provider").unwrap().is_none());
    }

    #[test]
    fn managed_oidc_tokens_preserve_access_token_and_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TokenCache {
            cache_dir: dir.path().to_path_buf(),
            key: [43u8; 32],
        };
        let jwt = make_jwt(r#"{"sub":"managed-user"}"#);

        cache
            .store_oidc_tokens("managed-crab.build", &jwt, "access", Some("refresh"), 600)
            .unwrap();
        let loaded = cache.load("managed-crab.build").unwrap().unwrap();

        assert_eq!(loaded.access_token.as_deref(), Some("access"));
        assert_eq!(loaded.expires_at, Some(loaded.issued_at + 600));
    }

    #[test]
    fn delete_nonexistent_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TokenCache {
            cache_dir: dir.path().to_path_buf(),
            key: [0u8; 32],
        };
        // Should not error.
        cache.delete("nonexistent").unwrap();
    }

    #[test]
    fn delete_all_clears_all_providers() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TokenCache {
            cache_dir: dir.path().to_path_buf(),
            key: [77u8; 32],
        };

        let jwt1 = make_jwt(r#"{"sub":"u1"}"#);
        let jwt2 = make_jwt(r#"{"sub":"u2"}"#);

        cache.store("provider-a", &jwt1, None).unwrap();
        cache.store("provider-b", &jwt2, None).unwrap();

        assert!(cache.load("provider-a").unwrap().is_some());
        assert!(cache.load("provider-b").unwrap().is_some());

        cache.delete_all().unwrap();

        assert!(cache.load("provider-a").unwrap().is_none());
        assert!(cache.load("provider-b").unwrap().is_none());
    }

    #[test]
    fn load_any_uses_first_available_provider_name() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TokenCache {
            cache_dir: dir.path().to_path_buf(),
            key: [44u8; 32],
        };

        let first_jwt = make_jwt(r#"{"sub":"first"}"#);
        let second_jwt = make_jwt(r#"{"sub":"second"}"#);

        cache.store("provider-b", &second_jwt, None).unwrap();
        let loaded = cache
            .load_any(&["provider-a", "provider-b"])
            .unwrap()
            .unwrap();
        assert_eq!(loaded.identity.subject, "second");

        cache.store("provider-a", &first_jwt, None).unwrap();
        let loaded = cache
            .load_any(&["provider-a", "provider-b"])
            .unwrap()
            .unwrap();
        assert_eq!(loaded.identity.subject, "first");
    }

    #[test]
    fn delete_all_names_clears_requested_provider_names() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TokenCache {
            cache_dir: dir.path().to_path_buf(),
            key: [45u8; 32],
        };

        let first_jwt = make_jwt(r#"{"sub":"first"}"#);
        let second_jwt = make_jwt(r#"{"sub":"second"}"#);
        cache.store("provider-a", &first_jwt, None).unwrap();
        cache.store("provider-b", &second_jwt, None).unwrap();

        cache
            .delete_all_names(&["provider-a", "provider-b"])
            .unwrap();

        assert!(cache.load("provider-a").unwrap().is_none());
        assert!(cache.load("provider-b").unwrap().is_none());
    }

    #[test]
    fn expand_token_cache_path_without_tilde_is_passthrough() {
        assert_eq!(
            expand_token_cache_path("/tmp/crab/tokens"),
            PathBuf::from("/tmp/crab/tokens")
        );
    }

    #[test]
    fn store_without_refresh_token() {
        let dir = tempfile::tempdir().unwrap();
        let cache = TokenCache {
            cache_dir: dir.path().to_path_buf(),
            key: [55u8; 32],
        };

        let jwt = make_jwt(r#"{"sub":"u1"}"#);
        cache.store("prov", &jwt, None).unwrap();

        let loaded = cache.load("prov").unwrap().unwrap();
        assert!(loaded.refresh_token.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_store_load_does_not_corrupt() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(TokenCache {
            cache_dir: dir.path().to_path_buf(),
            key: [88u8; 32],
        });

        let jwt_a = make_jwt(r#"{"sub":"task-a"}"#);
        let jwt_b = make_jwt(r#"{"sub":"task-b"}"#);

        let cache_a = Arc::clone(&cache);
        let jwt_a_clone = jwt_a.clone();
        let handle_a = tokio::spawn(async move {
            for _ in 0..20 {
                cache_a
                    .store("concurrent-prov", &jwt_a_clone, Some("refresh-a"))
                    .unwrap();
                let loaded = cache_a.load("concurrent-prov").unwrap();
                // The loaded token must be valid (from either task).
                assert!(loaded.is_some());
                let tokens = loaded.unwrap();
                assert!(tokens.identity.subject == "task-a" || tokens.identity.subject == "task-b");
            }
        });

        let cache_b = Arc::clone(&cache);
        let jwt_b_clone = jwt_b.clone();
        let handle_b = tokio::spawn(async move {
            for _ in 0..20 {
                cache_b
                    .store("concurrent-prov", &jwt_b_clone, Some("refresh-b"))
                    .unwrap();
                let loaded = cache_b.load("concurrent-prov").unwrap();
                assert!(loaded.is_some());
                let tokens = loaded.unwrap();
                assert!(tokens.identity.subject == "task-a" || tokens.identity.subject == "task-b");
            }
        });

        handle_a.await.unwrap();
        handle_b.await.unwrap();

        // After both tasks complete, the file should contain a valid token.
        let final_tokens = cache.load("concurrent-prov").unwrap().unwrap();
        assert!(
            final_tokens.identity.subject == "task-a" || final_tokens.identity.subject == "task-b"
        );
    }
}
