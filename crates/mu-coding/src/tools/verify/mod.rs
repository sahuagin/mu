//! `verify` — run an artifact in its REAL runtime and hand the runtime's
//! output back to the model (mu-lg8j1).
//!
//! From the mu-316wl phase-2 battery-1 finding: at constant model, lane
//! and prompt, the single feature that separated a playable artifact from
//! a crashing one was self-verification against the real runtime. mu's
//! models verified against a hand-built Node stub of the DOM, which
//! answers every call with a live object and is therefore blind to the
//! runtime-contract bugs that crashed 2/3 of the artifacts
//! (`null.getContext`, `fillStyle` on a string, a function referenced but
//! never defined). Every one of those surfaces the instant the page loads
//! in a real browser, as an uncaught exception — text. This tool makes
//! that loop one call instead of a dozen-turn improvisation.
//!
//! Kinds:
//! - `node` / `python`: run the file, return stdout/stderr/exit
//!   ([`runner`]).
//! - `web`: launch headless Chrome (local, or on another host over ssh),
//!   serve the artifact's directory to it over loopback ([`http`]), drive
//!   it over the DevTools protocol on Chrome's `--remote-debugging-pipe`
//!   ([`cdp`]), and report uncaught exceptions, console output, whether
//!   `requestAnimationFrame` ticks, whether pixels change (animation) and
//!   respond to scripted input, plus screenshot paths ([`web`]).
//!
//! Policy: [`SideEffects::Execute`] — it runs code whose effects are not
//! statically known. Permission is per-call `Ask` unless the session is
//! `--bash-yolo` (then `Allow`). That is STRICTER than default strict
//! bash, which is `Allow` behind an allowlist: verify has no allowlist to
//! gate content, so approval is the gate. Opt-in via `--tools verify`.
//!
//! The verdict (`PASS`/`FAIL`) is in the CONTENT; `is_error` is reserved
//! for the tool itself failing (missing artifact, no Chrome, probe
//! timeout). A crashing artifact is the feedback the model asked for, not
//! a tool error.

pub mod cdp;
pub mod http;
pub mod runner;
pub mod web;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use mu_core::agent::{
    PermissionLevel, RetryPolicy, SideEffects, Tool, ToolPolicy, ToolResult, ToolSpec,
};
use mu_core::config::VerifyConfig;
use serde_json::{json, Value};
use tokio::sync::oneshot;

/// Max bytes of report returned to the model (same cap as `bash`).
pub const OUTPUT_CAP_BYTES: usize = 64 * 1024;
pub const MAX_TIMEOUT_SECS: u64 = 600;
pub const MAX_SETTLE_SECS: u64 = 120;
/// Cap on scripted input events per call.
pub const MAX_INPUT_EVENTS: usize = 32;

/// Resolved runtime settings (config + defaults + probing).
#[derive(Debug, Clone)]
pub struct VerifySettings {
    /// Chrome binary (local path, or path on `chrome_ssh` host). `None`
    /// ⇒ probed at call time via [`probe_chrome`].
    pub chrome: Option<PathBuf>,
    pub chrome_ssh: Option<String>,
    pub chrome_args: Vec<String>,
    pub screenshot_dir: PathBuf,
    pub node: PathBuf,
    pub python: PathBuf,
    pub timeout: Duration,
    pub settle: Duration,
}

impl VerifySettings {
    pub fn from_config(cfg: &VerifyConfig) -> Self {
        Self {
            chrome: cfg.chrome.clone(),
            chrome_ssh: cfg.chrome_ssh.clone(),
            chrome_args: cfg.chrome_args.clone(),
            screenshot_dir: cfg
                .screenshot_dir
                .clone()
                .unwrap_or_else(default_screenshot_dir),
            node: cfg.node.clone().unwrap_or_else(|| PathBuf::from("node")),
            python: cfg
                .python
                .clone()
                .unwrap_or_else(|| PathBuf::from("python3")),
            timeout: Duration::from_secs(cfg.timeout_secs.clamp(1, MAX_TIMEOUT_SECS)),
            settle: Duration::from_secs(cfg.settle_secs.min(MAX_SETTLE_SECS)),
        }
    }
}

impl Default for VerifySettings {
    fn default() -> Self {
        Self::from_config(&VerifyConfig::default())
    }
}

fn default_screenshot_dir() -> PathBuf {
    std::env::temp_dir().join("mu-verify")
}

/// Find a Chrome binary when `[verify].chrome` is unset: `$CHROME`, the
/// user-level chrome-for-testing install, then the usual PATH names.
/// Local only — a remote (`chrome_ssh`) Chrome must be configured.
pub fn probe_chrome() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CHROME") {
        if !p.trim().is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Some(home) = dirs::home_dir() {
        let cft = home
            .join("chrome-test")
            .join("chrome-linux64")
            .join("chrome");
        if cft.is_file() {
            return Some(cft);
        }
    }
    let path = std::env::var_os("PATH")?;
    for name in ["google-chrome", "chromium", "chromium-browser", "chrome"] {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Node,
    Python,
    Web,
}

impl Kind {
    fn parse(s: &str) -> Result<Option<Self>, String> {
        match s {
            "auto" => Ok(None),
            "node" => Ok(Some(Kind::Node)),
            "python" => Ok(Some(Kind::Python)),
            "web" => Ok(Some(Kind::Web)),
            other => Err(format!(
                "verify: unknown kind '{other}' (expected auto, node, python, web)"
            )),
        }
    }

    /// `auto`: by extension.
    pub fn detect(artifact: &Path) -> Option<Self> {
        match artifact
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("js" | "mjs" | "cjs") => Some(Kind::Node),
            Some("py") => Some(Kind::Python),
            Some("html" | "htm") => Some(Kind::Web),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Node => "node",
            Kind::Python => "python",
            Kind::Web => "web",
        }
    }
}

/// Parsed, validated call arguments.
#[derive(Debug, Clone)]
pub struct Request {
    pub artifact: PathBuf,
    pub kind: Kind,
    pub args: Vec<String>,
    pub timeout: Duration,
    pub settle: Duration,
    pub input_events: Vec<web::InputEvent>,
}

#[derive(Debug)]
pub struct VerifyTool {
    settings: VerifySettings,
    permission: PermissionLevel,
}

impl VerifyTool {
    pub fn new(settings: VerifySettings, permission: PermissionLevel) -> Self {
        Self {
            settings,
            permission,
        }
    }

    pub fn settings(&self) -> &VerifySettings {
        &self.settings
    }

    /// Parse + validate arguments against the settings' defaults. Public
    /// so the dispatcher's `validate` and `execute` share one path.
    pub fn parse_request(&self, arguments: &Value) -> Result<Request, String> {
        let artifact = arguments
            .get("artifact")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "verify: missing required string argument 'artifact'".to_string())?;
        let artifact = PathBuf::from(artifact);
        let explicit = arguments
            .get("kind")
            .and_then(Value::as_str)
            .map(Kind::parse)
            .transpose()?
            .flatten();
        let kind = match explicit.or_else(|| Kind::detect(&artifact)) {
            Some(k) => k,
            None => {
                return Err(format!(
                    "verify: cannot infer kind from '{}' — pass kind = node | python | web",
                    artifact.display()
                ))
            }
        };
        let args: Vec<String> = match arguments.get("args") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| "verify: 'args' must be an array of strings".to_string())
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err("verify: 'args' must be an array of strings".to_string()),
        };
        let timeout = match arguments.get("timeout_secs") {
            None | Some(Value::Null) => self.settings.timeout,
            Some(v) => Duration::from_secs(
                v.as_u64()
                    .filter(|t| (1..=MAX_TIMEOUT_SECS).contains(t))
                    .ok_or_else(|| {
                        format!(
                            "verify: 'timeout_secs' must be an integer in 1..={MAX_TIMEOUT_SECS}"
                        )
                    })?,
            ),
        };
        let settle = match arguments.get("settle_secs") {
            None | Some(Value::Null) => self.settings.settle,
            Some(v) => {
                Duration::from_secs(v.as_u64().filter(|t| *t <= MAX_SETTLE_SECS).ok_or_else(
                    || format!("verify: 'settle_secs' must be an integer in 0..={MAX_SETTLE_SECS}"),
                )?)
            }
        };
        let input_events = match arguments.get("input_events") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                if items.len() > MAX_INPUT_EVENTS {
                    return Err(format!(
                        "verify: at most {MAX_INPUT_EVENTS} input_events per call"
                    ));
                }
                items
                    .iter()
                    .map(web::InputEvent::parse)
                    .collect::<Result<Vec<_>, _>>()?
            }
            Some(_) => return Err("verify: 'input_events' must be an array".to_string()),
        };
        if !input_events.is_empty() && kind != Kind::Web {
            return Err("verify: 'input_events' apply to kind = web only".to_string());
        }
        Ok(Request {
            artifact,
            kind,
            args,
            timeout,
            settle,
            input_events,
        })
    }
}

impl Tool for VerifyTool {
    fn spec(&self) -> ToolSpec {
        let chrome_note = match (&self.settings.chrome_ssh, &self.settings.chrome) {
            (Some(host), _) => format!("Headless Chrome runs on host '{host}' over ssh."),
            (None, Some(p)) => format!("Headless Chrome: {}.", p.display()),
            (None, None) => {
                "Headless Chrome is probed from $CHROME / the usual install paths.".to_string()
            }
        };
        ToolSpec {
            name: "verify".to_owned(),
            description: format!(
                "Run an artifact in its REAL runtime and get the runtime's output back. \
                 kind=node|python: runs the file, returns stdout/stderr/exit code. kind=web: loads \
                 the HTML in headless Chrome (real DOM + WebGL), returns uncaught exceptions with \
                 locations, console.error/log output, whether requestAnimationFrame ticks, whether \
                 the page animates and responds to scripted input_events, and screenshot paths. \
                 Use it after writing or editing an artifact instead of reasoning about whether \
                 it would run — a real browser throws the DOM-contract bugs (null.getContext, \
                 wrong types, undefined functions) that a stub cannot. Do NOT build your own \
                 browser harness (puppeteer/playwright/jsdom, npm installs, Node DOM stubs): that \
                 costs many turns and a stub misses exactly those bugs — this tool is one call. \
                 Typical loop: write the file → verify it → fix what threw → verify again. The \
                 first line is the verdict: PASS = loaded with zero uncaught exceptions; FAIL \
                 lists what threw. \
                 is_error is only set when the tool itself could not run (missing file, no \
                 runtime). {chrome_note} Output capped at 64KB. Default timeout {}s (max 600); \
                 web settle window {}s.",
                self.settings.timeout.as_secs(),
                self.settings.settle.as_secs(),
            ),
            display: None,
            when: None,
            policy: ToolPolicy {
                side_effects: SideEffects::Execute,
                permission: self.permission,
                retry: RetryPolicy::ModelDecides,
                required_aws_capability: None,
                idempotent: false,
                ends_turn_on_success: false,
            },
            verbatim_result: false,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "artifact": {
                        "type": "string",
                        "description": "Path to the file to run. Absolute, or relative to the daemon's working directory (the same directory bash commands run in); prefer absolute."
                    },
                    "kind": {
                        "type": "string",
                        "enum": ["auto", "node", "python", "web"],
                        "description": "Runtime. auto (default) picks by extension: .js/.mjs/.cjs → node, .py → python, .html/.htm → web."
                    },
                    "args": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Extra argv for node/python runs."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_TIMEOUT_SECS,
                        "description": "Cap on the run (node/python: the process; web: the probe). Teardown can add a few seconds beyond it."
                    },
                    "settle_secs": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": MAX_SETTLE_SECS,
                        "description": "web only: seconds to let the page run after load before sampling (world generation, asset decode)."
                    },
                    "input_events": {
                        "type": "array",
                        "description": "web only: scripted input dispatched after the settle window, so a game/app is exercised, not just booted. Items: {\"type\":\"click\",\"x\":640,\"y\":400}, {\"type\":\"key\",\"key\":\"w\",\"hold_ms\":500}, {\"type\":\"wait\",\"ms\":300}. Errors thrown during input are reported separately from boot errors.",
                        "items": {"type": "object"}
                    }
                },
                "required": ["artifact"]
            }),
        }
    }

    fn validate(&self, arguments: &Value) -> Result<(), String> {
        // Resolve the artifact here too: the dispatcher validates BEFORE
        // the approval gate, so a missing file is rejected without ever
        // prompting the operator for a doomed call.
        let request = self.parse_request(arguments)?;
        resolve_artifact(&request.artifact).map(|_| ())
    }

    fn execute<'life0, 'async_trait>(
        &'life0 self,
        arguments: Value,
        cancel_rx: oneshot::Receiver<()>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let request = self.parse_request(&arguments);
        let settings = self.settings.clone();
        Box::pin(async move {
            let request = match request {
                Ok(r) => r,
                Err(reason) => {
                    return ToolResult {
                        content: reason,
                        is_error: true,
                    }
                }
            };
            let artifact = match resolve_artifact(&request.artifact) {
                Ok(p) => p,
                Err(reason) => {
                    return ToolResult {
                        content: reason,
                        is_error: true,
                    }
                }
            };
            let work = run(settings, request.clone(), artifact);
            tokio::select! {
                outcome = work => match outcome {
                    Ok(report) => cap(report),
                    Err(e) => ToolResult {
                        content: cap_str(&format!("verify {}: {e:#}", request.kind.name())),
                        is_error: true,
                    },
                },
                _ = cancel_rx => ToolResult {
                    content: "verify cancelled".to_owned(),
                    is_error: true,
                },
            }
        })
    }
}

/// Resolve the artifact path and require a regular file — a clear error
/// beats a runtime's own "cannot find". Relative paths resolve against
/// the daemon's working directory, exactly as `bash` commands do (the
/// tool has no per-session cwd; the schema says so).
fn resolve_artifact(path: &Path) -> Result<PathBuf, String> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("verify: cannot resolve cwd: {e}"))?
            .join(path)
    };
    if !abs.is_file() {
        return Err(format!(
            "verify: artifact not found or not a file: {}",
            abs.display()
        ));
    }
    abs.canonicalize()
        .map_err(|e| format!("verify: cannot canonicalize {}: {e}", abs.display()))
}

async fn run(
    settings: VerifySettings,
    request: Request,
    artifact: PathBuf,
) -> anyhow::Result<ToolResult> {
    match request.kind {
        Kind::Node | Kind::Python => {
            let bin = if request.kind == Kind::Node {
                &settings.node
            } else {
                &settings.python
            };
            let cwd = artifact.parent().map(Path::to_path_buf).unwrap_or_default();
            let outcome =
                runner::run_script(bin, &artifact, &request.args, &cwd, request.timeout).await?;
            Ok(ToolResult {
                content: runner::format_report(request.kind.name(), &artifact, &outcome),
                is_error: false,
            })
        }
        Kind::Web => {
            let report = web::probe(
                &settings,
                &artifact,
                web::WebOptions {
                    timeout: request.timeout,
                    settle: request.settle,
                    input_events: request.input_events.clone(),
                },
            )
            .await?;
            Ok(ToolResult {
                content: web::format_report(&report),
                is_error: false,
            })
        }
    }
}

fn cap(mut result: ToolResult) -> ToolResult {
    result.content = cap_str(&result.content);
    result
}

fn cap_str(s: &str) -> String {
    if s.len() <= OUTPUT_CAP_BYTES {
        return s.to_owned();
    }
    let mut end = OUTPUT_CAP_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[verify: output truncated at {OUTPUT_CAP_BYTES} bytes]",
        &s[..end]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> VerifyTool {
        VerifyTool::new(VerifySettings::default(), PermissionLevel::Allow)
    }

    #[test]
    fn spec_is_execute_class_and_named_verify() {
        let s = tool().spec();
        assert_eq!(s.name, "verify");
        assert_eq!(s.policy.side_effects, SideEffects::Execute);
        assert!(!s.policy.idempotent);
        let ask = VerifyTool::new(VerifySettings::default(), PermissionLevel::Ask).spec();
        assert_eq!(ask.policy.permission, PermissionLevel::Ask);
    }

    #[test]
    fn kind_detection_by_extension() {
        assert_eq!(Kind::detect(Path::new("game.html")), Some(Kind::Web));
        assert_eq!(Kind::detect(Path::new("a/b/index.HTM")), Some(Kind::Web));
        assert_eq!(Kind::detect(Path::new("x.mjs")), Some(Kind::Node));
        assert_eq!(Kind::detect(Path::new("x.py")), Some(Kind::Python));
        assert_eq!(Kind::detect(Path::new("Makefile")), None);
    }

    #[test]
    fn validate_rejects_a_missing_artifact_before_any_prompt() {
        let t = tool();
        let e = t
            .validate(&json!({"artifact": "/definitely/not/here/x.js"}))
            .unwrap_err();
        assert!(e.contains("not found"), "{e}");
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ok.js");
        std::fs::write(&f, "1").unwrap();
        assert!(t
            .validate(&json!({"artifact": f.display().to_string()}))
            .is_ok());
    }

    #[test]
    fn parse_request_validates() {
        let t = tool();
        assert!(t.parse_request(&json!({})).map(|_| ()).is_err());
        assert!(t
            .parse_request(&json!({"artifact": "  "}))
            .map(|_| ())
            .is_err());
        assert!(t
            .parse_request(&json!({"artifact": "thing.bin"}))
            .map(|_| ())
            .unwrap_err()
            .contains("cannot infer kind"));
        assert!(t
            .parse_request(&json!({"artifact": "a.js", "kind": "rust"}))
            .map(|_| ())
            .is_err());
        assert!(t
            .parse_request(&json!({"artifact": "a.js", "timeout_secs": 0}))
            .map(|_| ())
            .is_err());
        assert!(t
            .parse_request(&json!({"artifact": "a.js", "timeout_secs": 601}))
            .map(|_| ())
            .is_err());
        assert!(t
            .parse_request(
                &json!({"artifact": "a.js", "input_events": [{"type":"click","x":1,"y":1}]})
            )
            .map(|_| ())
            .unwrap_err()
            .contains("web only"));
        assert!(t
            .parse_request(&json!({"artifact": "a.html", "input_events": [{"type":"teleport"}]}))
            .map(|_| ())
            .is_err());
        let r = t
            .parse_request(&json!({
                "artifact": "a.html", "settle_secs": 2, "timeout_secs": 30,
                "input_events": [{"type":"click","x":10,"y":20},{"type":"key","key":"w","hold_ms":100},{"type":"wait","ms":50}]
            }))
            .unwrap();
        assert_eq!(r.kind, Kind::Web);
        assert_eq!(r.settle, Duration::from_secs(2));
        assert_eq!(r.timeout, Duration::from_secs(30));
        assert_eq!(r.input_events.len(), 3);
        // explicit kind beats extension
        let r = t
            .parse_request(&json!({"artifact": "a.html", "kind": "node", "args": ["--x"]}))
            .unwrap();
        assert_eq!(r.kind, Kind::Node);
        assert_eq!(r.args, vec!["--x".to_string()]);
    }

    #[test]
    fn settings_from_config_apply_defaults_and_clamps() {
        let cfg = VerifyConfig {
            timeout_secs: 10_000,
            settle_secs: 999,
            node: Some(PathBuf::from("/opt/node/bin/node")),
            ..VerifyConfig::default()
        };
        let s = VerifySettings::from_config(&cfg);
        assert_eq!(s.timeout, Duration::from_secs(MAX_TIMEOUT_SECS));
        assert_eq!(s.settle, Duration::from_secs(MAX_SETTLE_SECS));
        assert_eq!(s.node, PathBuf::from("/opt/node/bin/node"));
        assert_eq!(s.python, PathBuf::from("python3"));
        assert!(s.screenshot_dir.ends_with("mu-verify"));
    }

    #[tokio::test]
    async fn missing_artifact_is_a_tool_error() {
        let (_tx, rx) = oneshot::channel();
        std::mem::forget(_tx);
        let r = tool()
            .execute(json!({"artifact": "/definitely/not/here/x.js"}), rx)
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("not found"), "{}", r.content);
    }

    #[test]
    fn cap_truncates_on_char_boundary() {
        let s = "é".repeat(OUTPUT_CAP_BYTES);
        let out = cap_str(&s);
        assert!(out.contains("truncated"));
        assert!(out.len() < s.len());
    }
}
