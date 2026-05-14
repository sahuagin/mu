//! Authentication primitives for providers that need OAuth.
//!
//! Today: OpenAI Codex (see `openai_codex` submodule). Future
//! additions might include Google Gemini CLI, GitHub Copilot, etc.
//! Anthropic is intentionally NOT here — per AGENTS.md, Anthropic's
//! OAuth flow remains subprocess-wrapped via `claude --print` to
//! avoid ToS friction.
//!
//! See spec mu-018.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod openai_codex;

/// An OAuth 2.0 token bundle. All fields are sensitive; the `Debug`
/// impl below redacts every secret. Don't add fields without
/// updating the Debug impl too.
#[derive(Clone, Serialize, Deserialize)]
pub struct OAuthToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub token_type: String,
    /// Unix seconds. `None` if the issuer doesn't provide expiry.
    pub expires_at: Option<u64>,
}

impl std::fmt::Debug for OAuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthToken")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("id_token", &self.id_token.as_ref().map(|_| "<redacted>"))
            .field("token_type", &self.token_type)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Pluggable storage for OAuth tokens.
pub trait TokenStore: Send + Sync {
    fn load(&self, provider: &str) -> Result<Option<OAuthToken>, AuthError>;
    fn save(&self, provider: &str, token: &OAuthToken) -> Result<(), AuthError>;
    fn remove(&self, provider: &str) -> Result<(), AuthError>;
}

/// Filesystem-backed `TokenStore`. Files are JSON, one per
/// provider, at `$base/<provider>.json` with mode `0600` (owner
/// read+write only).
pub struct FileSystemTokenStore {
    base_dir: PathBuf,
}

impl FileSystemTokenStore {
    /// Default location: `~/.config/mu/auth/`. Creates the dir if
    /// missing.
    pub fn default_location() -> Result<Self, AuthError> {
        let config = dirs::config_dir().ok_or(AuthError::NoConfigDir)?;
        let base_dir = config.join("mu").join("auth");
        if !base_dir.exists() {
            fs::create_dir_all(&base_dir)?;
        }
        Ok(Self { base_dir })
    }

    /// For tests: point at a chosen directory.
    pub fn with_base_dir(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    fn path_for(&self, provider: &str) -> PathBuf {
        self.base_dir.join(format!("{provider}.json"))
    }
}

impl TokenStore for FileSystemTokenStore {
    fn load(&self, provider: &str) -> Result<Option<OAuthToken>, AuthError> {
        let path = self.path_for(provider);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        let token: OAuthToken = serde_json::from_slice(&bytes)?;
        Ok(Some(token))
    }

    fn save(&self, provider: &str, token: &OAuthToken) -> Result<(), AuthError> {
        let path = self.path_for(provider);
        // Ensure base dir exists.
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }
        let bytes = serde_json::to_vec_pretty(token)?;
        write_with_restrictive_perms(&path, &bytes)?;
        Ok(())
    }

    fn remove(&self, provider: &str) -> Result<(), AuthError> {
        let path = self.path_for(provider);
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }
}

/// Write `bytes` to `path` with mode 0600 on Unix. On non-Unix
/// platforms, falls back to a plain write (with a `tracing::warn!`).
fn write_with_restrictive_perms(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tracing::warn!("non-Unix platform: token file written without explicit perms");
        fs::write(path, bytes)
    }
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("config dir not found")]
    NoConfigDir,
    #[error("oauth flow: {0}")]
    OAuthFlow(String),
    #[error("callback timeout")]
    CallbackTimeout,
    #[error("state mismatch — possible CSRF attempt; aborting login")]
    StateMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_token() -> OAuthToken {
        OAuthToken {
            access_token: "secret-access".to_owned(),
            refresh_token: Some("secret-refresh".to_owned()),
            id_token: Some("secret-id".to_owned()),
            token_type: "Bearer".to_owned(),
            expires_at: Some(1_900_000_000),
        }
    }

    #[test]
    fn b1_round_trip() {
        let dir = TempDir::new().unwrap();
        let store = FileSystemTokenStore::with_base_dir(dir.path().to_path_buf());
        let token = sample_token();
        store.save("openai-codex", &token).unwrap();
        let loaded = store.load("openai-codex").unwrap().unwrap();
        assert_eq!(loaded.access_token, token.access_token);
        assert_eq!(loaded.refresh_token, token.refresh_token);
        assert_eq!(loaded.id_token, token.id_token);
        assert_eq!(loaded.token_type, token.token_type);
        assert_eq!(loaded.expires_at, token.expires_at);
    }

    #[cfg(unix)]
    #[test]
    fn b2_file_perms_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let store = FileSystemTokenStore::with_base_dir(dir.path().to_path_buf());
        store.save("test", &sample_token()).unwrap();
        let meta = fs::metadata(dir.path().join("test.json")).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }

    #[test]
    fn b3_load_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        let store = FileSystemTokenStore::with_base_dir(dir.path().to_path_buf());
        assert!(store.load("nope").unwrap().is_none());
    }

    #[test]
    fn b4_remove_deletes_file() {
        let dir = TempDir::new().unwrap();
        let store = FileSystemTokenStore::with_base_dir(dir.path().to_path_buf());
        store.save("provider", &sample_token()).unwrap();
        assert!(store.load("provider").unwrap().is_some());
        store.remove("provider").unwrap();
        assert!(store.load("provider").unwrap().is_none());
        // remove on missing is also Ok.
        store.remove("provider").unwrap();
    }

    #[test]
    fn b5_debug_redacts_secrets() {
        let token = sample_token();
        let debug = format!("{token:?}");
        assert!(!debug.contains("secret-access"));
        assert!(!debug.contains("secret-refresh"));
        assert!(!debug.contains("secret-id"));
        assert!(debug.contains("<redacted>"));
        // Non-sensitive fields stay readable.
        assert!(debug.contains("Bearer"));
    }
}
