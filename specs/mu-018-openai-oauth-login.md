# Spec: OpenAI Codex OAuth login flow in mu

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-018                                         |
| status     | ready                                          |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

For mu to be a standalone agent, it has to do its own auth. The
prior AGENTS.md "no OAuth token holding" rule was overgeneralized —
the actual concern is Anthropic-specific. OpenAI Codex is an open
flow with public parameters; pi reimplements it, and we can too.

After mu-018: `mu login --provider openai-codex` opens a browser,
completes the OAuth flow, and stores tokens at
`~/.config/mu/auth/openai-codex.json`. The tokens are NOT yet
consumed — mu-019 (separate spec) rewires `OpenaiCodexProvider` to
use them, replacing the pi subprocess. mu-018 alone proves the
flow works and gives mu-019 a clean plug-in point.

CONVENTIONS apply.

## Scope

- **In:**
  - **`oauth2` crate** as workspace dep. Mature, handles PKCE flow,
    URL building, token exchange, refresh.
  - **`webbrowser` crate** as workspace dep. Opens the auth URL in
    the user's default browser. Cross-platform; falls back to
    "please open this URL manually" if no browser available.
  - **`mu-ai/src/auth/mod.rs`** — `OAuthToken` struct, `TokenStore`
    trait, `FileSystemTokenStore` impl (writes to
    `~/.config/mu/auth/<provider>.json` with `0600` perms).
  - **`mu-ai/src/auth/openai_codex.rs`** — OpenAI Codex flow
    specifically. Constants pulled from pi's auth.rs (verified
    working). `login_flow()` runs the full PKCE dance; 
    `refresh_token()` handles refresh.
  - **`mu-coding/src/bin/mu.rs`** — `Command::Login { provider }`
    and `Command::Logout { provider }` subcommands.
  - Local HTTP callback server: small `tokio::net::TcpListener` with
    manual response writing (no axum/hyper bloat for ~50 lines of
    logic).
  - Tests: unit-testable parts (URL construction, JSON
    round-trips, file permissions). The full live OAuth flow is
    interactive and can't be unit-tested; manual smoke is the
    integration check.

- **Out:**
  - **Using the stored tokens.** mu-018 stores tokens; mu-019 rewires
    `OpenaiCodexProvider` to consume them and drop the pi subprocess.
  - **Token encryption at rest.** Plaintext JSON with `0600` perms
    is the v1. Encryption (via keyring service or similar) is a
    future hardening spec.
  - **`--ephemeral` flag.** Only meaningful when the token is *used*
    (mu-019). For mu-018 alone there's no in-memory mode that's
    useful — tokens stored in the login process's memory die when
    the process exits.
  - **Other providers' OAuth.** Anthropic OAuth is deferred (ToS).
    Google Gemini OAuth, GitHub Copilot OAuth — future specs when
    we add those providers.
  - **Token rotation policy.** v1 doesn't proactively rotate; tokens
    refresh on demand when access_token has expired (handled by
    mu-019). For mu-018, the login flow obtains a fresh token set,
    overwriting any prior one for that provider.

## Invariants

- **INV-1 (CONVENTIONS apply).**
- **INV-2 (per-provider OAuth posture per AGENTS.md).** Anthropic
  stays subprocess-wrap. OpenAI Codex direct.
- **INV-3 (file perms).** Token files must be created with mode
  `0600` (owner read+write only) on Unix. Tested via metadata check.
- **INV-4 (no plaintext secrets in logs).** Access tokens, refresh
  tokens, and id tokens must NEVER appear in `tracing::*` output or
  Display/Debug impls. The token types should derive `Debug` only
  with field-level redaction, or implement Debug manually.
- **INV-5 (state check).** The OAuth `state` parameter is randomly
  generated per login (32+ bytes); the callback MUST verify it
  matches before exchanging the code. Defends against CSRF.

## Interfaces

### `mu-ai/src/auth/mod.rs`

```rust
use std::path::PathBuf;

/// An OAuth 2.0 token bundle. All fields are sensitive; Debug
/// impl redacts.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct OAuthToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub token_type: String,
    /// Unix seconds; None if the issuer doesn't provide expiry.
    pub expires_at: Option<u64>,
}

impl std::fmt::Debug for OAuthToken {
    // Custom: redact every secret field.
}

/// Pluggable storage for OAuth tokens.
pub trait TokenStore: Send + Sync {
    fn load(&self, provider: &str) -> Result<Option<OAuthToken>, AuthError>;
    fn save(&self, provider: &str, token: &OAuthToken) -> Result<(), AuthError>;
    fn remove(&self, provider: &str) -> Result<(), AuthError>;
}

/// Default impl: filesystem-backed JSON files at
/// `$base/provider.json` with 0600 perms.
pub struct FileSystemTokenStore {
    base_dir: PathBuf,
}

impl FileSystemTokenStore {
    /// Default location: `~/.config/mu/auth/`.
    pub fn default_location() -> Result<Self, AuthError>;
    pub fn with_base_dir(base_dir: PathBuf) -> Self;
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("io: {0}")] Io(#[from] std::io::Error),
    #[error("serde: {0}")] Serde(#[from] serde_json::Error),
    #[error("config dir not found")] NoConfigDir,
    #[error("oauth flow: {0}")] OAuthFlow(String),
    #[error("callback timeout")] CallbackTimeout,
    #[error("state mismatch")] StateMismatch,
}
```

### `mu-ai/src/auth/openai_codex.rs`

```rust
// Constants — copied verbatim from pi's auth.rs.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const SCOPES: &str = "openid profile email offline_access";
const CALLBACK_PORT: u16 = 1455;

/// Run the full OAuth flow:
///   1. Generate PKCE verifier + challenge.
///   2. Generate random state (CSRF defense).
///   3. Build authorize URL with codex-specific extras
///      (codex_cli_simplified_flow=true, originator=mu).
///   4. Open browser to authorize URL.
///   5. Spawn callback server on localhost:1455, wait up to 10
///      minutes for the redirect.
///   6. Verify state matches.
///   7. Exchange code for tokens.
///   8. Return `OAuthToken`.
pub async fn login_flow() -> Result<OAuthToken, AuthError>;

/// Refresh using a stored refresh_token. Returns the new token
/// bundle (refresh tokens may also rotate).
pub async fn refresh_access_token(refresh_token: &str)
    -> Result<OAuthToken, AuthError>;
```

### `mu-coding/src/bin/mu.rs` — new subcommands

```rust
enum Command {
    // ... existing ...
    /// Authenticate with a provider that requires OAuth.
    Login {
        /// Provider name (currently: openai-codex).
        #[arg(long)]
        provider: String,
    },
    /// Remove stored credentials for a provider.
    Logout {
        #[arg(long)]
        provider: String,
    },
}
```

Login arm:
```rust
Command::Login { provider } => {
    match provider.as_str() {
        "openai-codex" => {
            let store = FileSystemTokenStore::default_location()?;
            let token = mu_ai::auth::openai_codex::login_flow().await?;
            store.save("openai-codex", &token)?;
            println!("Logged in to OpenAI Codex. Token stored at \
                      ~/.config/mu/auth/openai-codex.json (0600).");
            Ok(())
        }
        other => anyhow::bail!("login not supported for provider: {other}"),
    }
}
```

Logout arm: equivalent shape; removes the file.

## Behaviors

1. **B-1 (FileSystemTokenStore round-trip):** Save a token, load it
   back, assert equality. Use a tempdir.

2. **B-2 (file permissions are 0600):** After save, stat the file
   and check the mode. (Unix only; the test skips silently on
   Windows.)

3. **B-3 (load returns None for missing file):** `store.load("nope")`
   when no file exists returns `Ok(None)`, not an error.

4. **B-4 (remove deletes the file):** Save, remove, then load
   returns `Ok(None)`.

5. **B-5 (Debug redacts secrets):** `format!("{:?}", token)` does
   not contain the access_token, refresh_token, or id_token values.

6. **B-6 (PKCE verifier shape):** generated verifier is 43-128 chars,
   URL-safe-base64-encoded. (Verified by oauth2 crate; we just
   smoke-test that the round-trip produces parseable code_challenge.)

7. **B-7 (authorize URL contains required params):** Build the URL
   for codex's flow; assert it contains
   `codex_cli_simplified_flow=true`, `originator=mu`, the right
   redirect_uri, the right scopes.

8. **B-8 (state mismatch rejects):** Construct a fake callback with
   wrong state; assert `StateMismatch` error.

9. **B-9 (callback server picks up the code):** Spin up the callback
   listener on a test port (not 1455); make an HTTP request with
   `?code=test_code&state=<expected>`; assert the listener resolves
   with `Ok(("test_code", "<expected>"))`.

10. **(Manual smoke)** `mu login --provider openai-codex` opens
    browser, completes flow, prints success. NOT in CI — interactive.

## Acceptance

- New files:
  - `crates/mu-ai/src/auth/mod.rs`
  - `crates/mu-ai/src/auth/openai_codex.rs`
- Modified files:
  - `Cargo.toml` (workspace) — add `oauth2`, `webbrowser` to
    `[workspace.dependencies]`
  - `crates/mu-ai/Cargo.toml` — pull in those deps
  - `crates/mu-ai/src/lib.rs` — `pub mod auth;` + re-exports
  - `crates/mu-coding/src/bin/mu.rs` — `Login` / `Logout`
    subcommands
- `cargo build` clean.
- `cargo nextest run` passes — every existing test plus B-1..B-9.
  B-10 is manual.
- Manual: `mu login --provider openai-codex` from the user's machine
  completes the full flow and writes the token file. (Verified by
  user after the implementation lands.)

## Out-of-circuit warnings

- **OOC-1:** `oauth2` crate has had multiple major version bumps
  recently. Pin to a specific version in `[workspace.dependencies]`;
  document the version we picked in case future updates break the
  API.

- **OOC-2:** The redirect URI is `http://localhost:1455/auth/callback`.
  This port number is REGISTERED at OpenAI's end as part of the
  Codex CLI app registration. Don't try to use a different port —
  OpenAI will reject the callback. If port 1455 is in use locally,
  fail with a clear message ("port 1455 is in use; can't start
  callback listener; close whatever is holding it and retry").

- **OOC-3:** The custom `Debug` impl for `OAuthToken` must redact
  ALL three secret fields (access_token, refresh_token, id_token).
  Easy to forget when adding a new field; B-5 catches obvious
  cases but a vigilant reviewer is the real safety net.

- **OOC-4:** Browser-opening is best-effort. If `webbrowser::open`
  fails, fall back to `eprintln!("Please open: {url}")` and STILL
  start the callback listener. Some headless environments have no
  browser; users can copy the URL to another machine and complete
  the flow.

- **OOC-5:** The OpenAI Codex flow uses these extra parameters
  beyond standard OAuth: `codex_cli_simplified_flow=true`,
  `originator=<client_name>`, `id_token_add_organizations=true`.
  All three are non-standard and required for the simplified flow
  pi uses. `originator` would be "mu" rather than pi's "pi" — it's
  a free string identifying which client is asking.

## Prior work / context

- pi's auth.rs — constants, parameter list, flow shape. Reference
  only; we write fresh code using the `oauth2` crate.
- `oauth2` crate docs — PKCE flow primitives.
- task_log `75cc15e2` — the override entry for the AGENTS.md
  no-OAuth-token-holding rule.
- mu-019 (planned) — rewire `OpenaiCodexProvider` to consume the
  stored tokens.

## Changelog

- 2026-05-10 — initial draft (claude-personal).
