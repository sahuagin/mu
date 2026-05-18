use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use mu_core::agent::{
    PermissionLevel, RetryPolicy, SideEffects, Tool, ToolPolicy, ToolResult, ToolSpec,
};
use mu_core::aws::{AwsCapabilityCatalog, AwsCapabilityCatalogEntry, AwsCatalogError};
use mu_core::capability::AwsCapability;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::time;

const DEFAULT_CAPABILITY: &str = "aws.scout.readonly";
const DEFAULT_CALL_TIMEOUT_SECS: u64 = 45;
const MAX_CALL_TIMEOUT_SECS: u64 = 600;
const DEFAULT_RECON_CALL_COUNT_BUDGET: u64 = 20;
const RUNNER_TIMEOUT_GRACE_SECS: u64 = 30;
const MAX_RUNNER_TIMEOUT_SECS: u64 = 14_400;
const MAX_RUNNER_STREAM_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct AwsReconTool {
    catalog: AwsCapabilityCatalog,
    catalog_digest: String,
    fixture_dir: Option<PathBuf>,
    runner: Option<AwsReconRunner>,
}

#[derive(Debug, Clone)]
struct AwsReconRunner {
    runner_path: PathBuf,
    script_path: PathBuf,
    cwd: Option<PathBuf>,
}

impl AwsReconTool {
    pub fn new(
        catalog: AwsCapabilityCatalog,
        catalog_digest: impl Into<String>,
        fixture_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            catalog,
            catalog_digest: catalog_digest.into(),
            fixture_dir,
            runner: None,
        }
    }

    pub fn with_runner(
        catalog: AwsCapabilityCatalog,
        catalog_digest: impl Into<String>,
        runner_path: impl Into<PathBuf>,
        script_path: impl Into<PathBuf>,
        cwd: Option<PathBuf>,
    ) -> Self {
        Self {
            catalog,
            catalog_digest: catalog_digest.into(),
            fixture_dir: None,
            runner: Some(AwsReconRunner {
                runner_path: runner_path.into(),
                script_path: script_path.into(),
                cwd,
            }),
        }
    }

    pub fn from_env() -> Result<Self, String> {
        let catalog_path = std::env::var("MU_AWS_CAPABILITY_CATALOG")
            .map_err(|_| "MU_AWS_CAPABILITY_CATALOG is required for aws_recon".to_owned())?;
        let catalog_bytes = std::fs::read(&catalog_path)
            .map_err(|e| format!("failed to read AWS capability catalog {catalog_path}: {e}"))?;
        let catalog: AwsCapabilityCatalog = serde_json::from_slice(&catalog_bytes)
            .map_err(|e| format!("failed to parse AWS capability catalog {catalog_path}: {e}"))?;
        let digest = format!("sha256:{}", sha256_hex(&catalog_bytes));
        let fixture_dir = std::env::var("MU_AWS_RECON_FIXTURE_DIR")
            .ok()
            .map(PathBuf::from);
        let runner_path = std::env::var("MU_AWS_RECON_RUNNER").ok().map(PathBuf::from);
        let script_path = std::env::var("MU_AWS_RECON_SCRIPT").ok().map(PathBuf::from);
        let cwd = std::env::var("MU_AWS_RECON_CWD").ok().map(PathBuf::from);

        match (fixture_dir, runner_path, script_path) {
            (Some(fixture_dir), _, _) => Ok(Self::new(catalog, digest, Some(fixture_dir))),
            (None, Some(runner_path), Some(script_path)) => Ok(Self::with_runner(
                catalog,
                digest,
                runner_path,
                script_path,
                cwd,
            )),
            (None, None, None) => Ok(Self::new(catalog, digest, None)),
            (None, _, _) => Err(
                "MU_AWS_RECON_RUNNER and MU_AWS_RECON_SCRIPT must be set together for live aws_recon"
                    .to_owned(),
            ),
        }
    }
}

impl Tool for AwsReconTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "aws_recon",
            "Run read-only AWS sandbox reconnaissance through the Mu AWS capability stack. Uses fixture mode when configured with MU_AWS_RECON_FIXTURE_DIR, or an explicit local runner when MU_AWS_RECON_RUNNER and MU_AWS_RECON_SCRIPT are set.",
            json!({
                "type": "object",
                "properties": {
                    "capability": {
                        "type": "string",
                        "description": "AWS capability name to use. Defaults to aws.scout.readonly.",
                        "default": DEFAULT_CAPABILITY
                    },
                    "call_timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_CALL_TIMEOUT_SECS,
                        "description": "Per-AWS-call timeout. Defaults to 45 seconds."
                    },
                    "output_dir": {
                        "type": "string",
                        "description": "Optional output directory for future runner-backed execution. Ignored in fixture mode."
                    },
                    "runner_timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_RUNNER_TIMEOUT_SECS,
                        "description": "Outer timeout for the runner subprocess. Defaults to call_timeout_secs * 20 + 30 seconds because call_timeout_secs is per AWS call."
                    }
                }
            }),
        )
        .with_policy(ToolPolicy {
            side_effects: SideEffects::External,
            permission: PermissionLevel::Allow,
            retry: RetryPolicy::ModelDecides,
            required_aws_capability: Some(DEFAULT_CAPABILITY.to_owned()),
            idempotent: false,
        })
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
        Box::pin(async move {
            tokio::select! {
                result = async { self.execute_inner(arguments).await } => result,
                _ = cancel_rx => ToolResult {
                    content: structured_error("cancelled", DEFAULT_CAPABILITY, &self.catalog_digest, None, "aws_recon cancelled"),
                    is_error: true,
                },
            }
        })
    }
}

impl AwsReconTool {
    async fn execute_inner(&self, arguments: Value) -> ToolResult {
        let capability_name = match capability_argument(&arguments) {
            Ok(c) => c,
            Err(result) => return result,
        };
        let call_timeout_secs = match call_timeout_argument(&arguments) {
            Ok(t) => t,
            Err(result) => return result,
        };
        let output_dir = match output_dir_argument(&arguments) {
            Ok(path) => path,
            Err(result) => return result,
        };
        let runner_timeout_secs = match runner_timeout_argument(&arguments, call_timeout_secs) {
            Ok(timeout) => timeout,
            Err(result) => return result,
        };

        if capability_name != DEFAULT_CAPABILITY {
            return ToolResult {
                content: structured_error(
                    "unsupported_capability",
                    &capability_name,
                    &self.catalog_digest,
                    None,
                    "aws_recon fixture skeleton only supports aws.scout.readonly",
                ),
                is_error: true,
            };
        }

        let requested = AwsCapability {
            name: capability_name.clone(),
            session_policy: None,
        };
        let entry = match self.catalog.resolve_materialized(&requested) {
            Ok(entry) => entry,
            Err(err) => {
                return ToolResult {
                    content: catalog_error(&capability_name, &self.catalog_digest, err),
                    is_error: true,
                };
            }
        };

        if entry.mutation_allowed {
            return ToolResult {
                content: structured_error(
                    "mutation_capability_rejected",
                    &capability_name,
                    &self.catalog_digest,
                    Some(entry),
                    "aws_recon requires a read-only AWS capability",
                ),
                is_error: true,
            };
        }

        if let Some(fixture_dir) = &self.fixture_dir {
            let report = json!({
                "kind": "aws_recon_report",
                "mode": "fixture",
                "artifact_dir": fixture_dir,
                "summary_path": fixture_dir.join("summary.json"),
                "capability": capability_name,
                "call_timeout_secs": call_timeout_secs,
                "catalog_digest": self.catalog_digest,
                "audit": audit_metadata(DEFAULT_CAPABILITY, &self.catalog_digest, Some(entry)),
                "sts": null,
            });
            return ToolResult {
                content: serde_json::to_string_pretty(&report)
                    .expect("json serialization cannot fail"),
                is_error: false,
            };
        }

        let runner = match &self.runner {
            Some(runner) => runner,
            None => {
                return ToolResult {
                    content: structured_error(
                        "runner_not_configured",
                        &capability_name,
                        &self.catalog_digest,
                        Some(entry),
                        "aws_recon live runner is not configured; set MU_AWS_RECON_RUNNER plus MU_AWS_RECON_SCRIPT, or MU_AWS_RECON_FIXTURE_DIR for fixture mode",
                    ),
                    is_error: true,
                };
            }
        };

        self.execute_runner(
            runner,
            &capability_name,
            call_timeout_secs,
            runner_timeout_secs,
            output_dir,
            entry,
        )
        .await
    }

    async fn execute_runner(
        &self,
        runner: &AwsReconRunner,
        capability_name: &str,
        call_timeout_secs: u64,
        runner_timeout_secs: u64,
        output_dir: Option<PathBuf>,
        entry: &AwsCapabilityCatalogEntry,
    ) -> ToolResult {
        let mut command = Command::new(&runner.runner_path);
        command
            .arg(capability_name)
            .arg("--")
            .arg(&runner.script_path)
            .arg("--call-timeout")
            .arg(call_timeout_secs.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(output_dir) = &output_dir {
            command.arg("--out-dir").arg(output_dir);
        }
        if let Some(cwd) = &runner.cwd {
            command.current_dir(cwd);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                return ToolResult {
                    content: structured_error(
                        "runner_spawn_failed",
                        capability_name,
                        &self.catalog_digest,
                        Some(entry),
                        &format!("failed to spawn aws_recon runner: {err}"),
                    ),
                    is_error: true,
                };
            }
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_task = tokio::spawn(async move { read_limited(stdout).await });
        let stderr_task = tokio::spawn(async move { read_limited(stderr).await });

        let status =
            match time::timeout(Duration::from_secs(runner_timeout_secs), child.wait()).await {
                Ok(Ok(status)) => status,
                Ok(Err(err)) => {
                    return ToolResult {
                        content: structured_error(
                            "runner_wait_failed",
                            capability_name,
                            &self.catalog_digest,
                            Some(entry),
                            &format!("failed waiting for aws_recon runner: {err}"),
                        ),
                        is_error: true,
                    };
                }
                Err(_) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return ToolResult {
                        content: structured_error(
                            "runner_timeout",
                            capability_name,
                            &self.catalog_digest,
                            Some(entry),
                            &format!(
                                "aws_recon runner exceeded outer timeout of {runner_timeout_secs}s"
                            ),
                        ),
                        is_error: true,
                    };
                }
            };

        let stdout_capture = await_capture(stdout_task).await;
        let stderr_capture = await_capture(stderr_task).await;

        if status.success() {
            match parse_runner_summary(&stdout_capture.text) {
                Ok(parsed) => {
                    let report_dir = parsed.report_dir;
                    let report = json!({
                        "kind": "aws_recon_report",
                        "mode": "runner",
                        "report_dir": report_dir,
                        "summary_path": format!("{report_dir}/summary.json"),
                        "capability": capability_name,
                        "call_timeout_secs": call_timeout_secs,
                        "runner_timeout_secs": runner_timeout_secs,
                        "catalog_digest": self.catalog_digest,
                        "audit": audit_metadata(capability_name, &self.catalog_digest, Some(entry)),
                        "runner": {
                            "path": runner.runner_path,
                            "script": runner.script_path,
                        },
                        "stdout_summary": parsed.value,
                        "output_capture": {
                            "stdout_truncated": stdout_capture.truncated,
                            "stderr_truncated": stderr_capture.truncated,
                            "limit_bytes": MAX_RUNNER_STREAM_BYTES,
                        },
                    });
                    ToolResult {
                        content: serde_json::to_string_pretty(&report)
                            .expect("json serialization cannot fail"),
                        is_error: false,
                    }
                }
                Err(message) => ToolResult {
                    content: structured_error(
                        "runner_output_parse_failed",
                        capability_name,
                        &self.catalog_digest,
                        Some(entry),
                        &message,
                    ),
                    is_error: true,
                },
            }
        } else {
            ToolResult {
                content: structured_error(
                    "runner_failed",
                    capability_name,
                    &self.catalog_digest,
                    Some(entry),
                    &format!(
                        "aws_recon runner exited with {status}: {}",
                        stderr_capture.text.trim()
                    ),
                ),
                is_error: true,
            }
        }
    }
}

fn capability_argument(arguments: &Value) -> Result<String, ToolResult> {
    match arguments.get("capability") {
        None => Ok(DEFAULT_CAPABILITY.to_owned()),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_owned()),
        Some(_) => Err(ToolResult {
            content: structured_error(
                "invalid_capability_argument",
                DEFAULT_CAPABILITY,
                "unknown",
                None,
                "`capability` must be a non-empty string",
            ),
            is_error: true,
        }),
    }
}

fn runner_timeout_argument(arguments: &Value, call_timeout_secs: u64) -> Result<u64, ToolResult> {
    match arguments.get("runner_timeout_secs") {
        None => Ok(default_runner_timeout_secs(call_timeout_secs)),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(v) if (1..=MAX_RUNNER_TIMEOUT_SECS).contains(&v) => Ok(v),
            _ => Err(ToolResult {
                content: structured_error(
                    "invalid_runner_timeout_secs",
                    DEFAULT_CAPABILITY,
                    "unknown",
                    None,
                    "`runner_timeout_secs` must be an integer between 1 and 14400",
                ),
                is_error: true,
            }),
        },
        Some(_) => Err(ToolResult {
            content: structured_error(
                "invalid_runner_timeout_secs",
                DEFAULT_CAPABILITY,
                "unknown",
                None,
                "`runner_timeout_secs` must be an integer between 1 and 14400",
            ),
            is_error: true,
        }),
    }
}

fn default_runner_timeout_secs(call_timeout_secs: u64) -> u64 {
    call_timeout_secs
        .saturating_mul(DEFAULT_RECON_CALL_COUNT_BUDGET)
        .saturating_add(RUNNER_TIMEOUT_GRACE_SECS)
        .min(MAX_RUNNER_TIMEOUT_SECS)
}

fn output_dir_argument(arguments: &Value) -> Result<Option<PathBuf>, ToolResult> {
    match arguments.get("output_dir") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(PathBuf::from(s.trim()))),
        Some(_) => Err(ToolResult {
            content: structured_error(
                "invalid_output_dir",
                DEFAULT_CAPABILITY,
                "unknown",
                None,
                "`output_dir` must be a non-empty string when provided",
            ),
            is_error: true,
        }),
    }
}

fn call_timeout_argument(arguments: &Value) -> Result<u64, ToolResult> {
    match arguments.get("call_timeout_secs") {
        None => Ok(DEFAULT_CALL_TIMEOUT_SECS),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(v) if (1..=MAX_CALL_TIMEOUT_SECS).contains(&v) => Ok(v),
            _ => Err(ToolResult {
                content: structured_error(
                    "invalid_call_timeout_secs",
                    DEFAULT_CAPABILITY,
                    "unknown",
                    None,
                    "`call_timeout_secs` must be an integer between 1 and 600",
                ),
                is_error: true,
            }),
        },
        Some(_) => Err(ToolResult {
            content: structured_error(
                "invalid_call_timeout_secs",
                DEFAULT_CAPABILITY,
                "unknown",
                None,
                "`call_timeout_secs` must be an integer between 1 and 600",
            ),
            is_error: true,
        }),
    }
}

#[derive(Debug, Clone, PartialEq)]
struct RunnerSummary {
    value: Value,
    report_dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamCapture {
    text: String,
    truncated: bool,
}

async fn read_limited<R>(reader: Option<R>) -> StreamCapture
where
    R: AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return StreamCapture {
            text: String::new(),
            truncated: false,
        };
    };
    let mut buf = [0_u8; 8192];
    let mut captured = Vec::new();
    let mut truncated = false;
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if captured.len() < MAX_RUNNER_STREAM_BYTES {
            let remaining = MAX_RUNNER_STREAM_BYTES - captured.len();
            let keep = remaining.min(n);
            captured.extend_from_slice(&buf[..keep]);
            if keep < n {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    StreamCapture {
        text: String::from_utf8_lossy(&captured).into_owned(),
        truncated,
    }
}

async fn await_capture(handle: tokio::task::JoinHandle<StreamCapture>) -> StreamCapture {
    handle.await.unwrap_or(StreamCapture {
        text: String::new(),
        truncated: true,
    })
}

fn parse_runner_summary(stdout: &str) -> Result<RunnerSummary, String> {
    let start = stdout
        .find('{')
        .ok_or_else(|| "aws_recon runner stdout did not contain a JSON summary".to_owned())?;
    let end = stdout
        .rfind('}')
        .ok_or_else(|| "aws_recon runner stdout JSON summary was incomplete".to_owned())?;
    let value: Value = serde_json::from_str(&stdout[start..=end])
        .map_err(|err| format!("failed to parse aws_recon runner summary JSON: {err}"))?;
    let report_dir = value
        .get("report")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            "aws_recon runner summary missing non-empty string field `report`".to_owned()
        })?
        .to_owned();
    require_array_field(&value, "errors")?;
    require_array_field(&value, "findings")?;
    Ok(RunnerSummary { value, report_dir })
}

fn require_array_field(value: &Value, field: &str) -> Result<(), String> {
    match value.get(field) {
        Some(Value::Array(_)) => Ok(()),
        _ => Err(format!(
            "aws_recon runner summary missing array field `{field}`"
        )),
    }
}

fn catalog_error(capability: &str, catalog_digest: &str, err: AwsCatalogError) -> String {
    let reason = match err {
        AwsCatalogError::UnknownCapability { .. } => "catalog_unknown_capability",
        AwsCatalogError::CapabilityNotMaterialized { .. } => "catalog_capability_not_materialized",
    };
    structured_error(reason, capability, catalog_digest, None, &err.to_string())
}

fn structured_error(
    reason: &str,
    capability: &str,
    catalog_digest: &str,
    entry: Option<&AwsCapabilityCatalogEntry>,
    message: &str,
) -> String {
    serde_json::to_string_pretty(&json!({
        "kind": "aws_recon_refusal",
        "reason": reason,
        "message": message,
        "audit": audit_metadata(capability, catalog_digest, entry),
    }))
    .expect("json serialization cannot fail")
}

fn audit_metadata(
    capability: &str,
    catalog_digest: &str,
    entry: Option<&AwsCapabilityCatalogEntry>,
) -> Value {
    json!({
        "mu_session_id": null,
        "tool_call_id": null,
        "capability": capability,
        "catalog_digest": catalog_digest,
        "aws_profile": entry.and_then(|e| e.aws_profile.as_deref()),
        "role_arn": entry.and_then(|e| e.role_arn.as_deref()),
        "sts": null,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::AsyncWriteExt;

    fn catalog_with(entries: Value) -> AwsCapabilityCatalog {
        serde_json::from_value(json!({
            "schema_version": 1,
            "default_region": "us-east-1",
            "capabilities": entries,
        }))
        .expect("catalog fixture parses")
    }

    fn readonly_catalog() -> AwsCapabilityCatalog {
        catalog_with(json!({
            "aws.scout.readonly": {
                "description": "Read-only scout.",
                "aws_profile": "mu-readonly-scout",
                "role_name": "mu-readonly-scout",
                "role_arn": "arn:aws:iam::123456789012:role/mu-readonly-scout",
                "mutation_allowed": false,
                "managed_policies": ["arn:aws:iam::aws:policy/ReadOnlyAccess"]
            }
        }))
    }

    async fn execute(tool: &AwsReconTool, arguments: Value) -> ToolResult {
        let (_cancel_tx, cancel_rx) = oneshot::channel();
        tool.execute(arguments, cancel_rx).await
    }

    #[test]
    fn spec_declares_required_aws_capability() {
        let tool = AwsReconTool::new(readonly_catalog(), "sha256:test", None);
        let spec = tool.spec();

        assert_eq!(spec.name, "aws_recon");
        assert_eq!(
            spec.policy.required_aws_capability.as_deref(),
            Some("aws.scout.readonly")
        );
        assert_eq!(spec.policy.side_effects, SideEffects::External);
        assert_eq!(
            spec.input_schema["properties"]["capability"]["default"],
            DEFAULT_CAPABILITY
        );
    }

    #[tokio::test]
    async fn catalog_miss_is_structured_error() {
        let tool = AwsReconTool::new(
            catalog_with(json!({})),
            "sha256:test",
            Some("fixture".into()),
        );
        let result = execute(&tool, json!({})).await;
        let value: Value = serde_json::from_str(&result.content).expect("structured json");

        assert!(result.is_error);
        assert_eq!(value["kind"], "aws_recon_refusal");
        assert_eq!(value["reason"], "catalog_unknown_capability");
        assert_eq!(value["audit"]["capability"], "aws.scout.readonly");
    }

    #[tokio::test]
    async fn mutating_catalog_entry_is_refused() {
        let catalog = catalog_with(json!({
            "aws.scout.readonly": {
                "description": "Bad mutating scout.",
                "aws_profile": "mu-sandbox-builder",
                "role_name": "mu-sandbox-builder",
                "role_arn": "arn:aws:iam::123456789012:role/mu-sandbox-builder",
                "mutation_allowed": true
            }
        }));
        let tool = AwsReconTool::new(catalog, "sha256:test", Some("fixture".into()));
        let result = execute(&tool, json!({})).await;
        let value: Value = serde_json::from_str(&result.content).expect("structured json");

        assert!(result.is_error);
        assert_eq!(value["reason"], "mutation_capability_rejected");
        assert_eq!(value["audit"]["aws_profile"], "mu-sandbox-builder");
    }

    #[tokio::test]
    async fn fixture_mode_returns_recon_report_shape() {
        let tool = AwsReconTool::new(
            readonly_catalog(),
            "sha256:test",
            Some("/tmp/mu-fixture".into()),
        );
        let result = execute(&tool, json!({"call_timeout_secs": 60})).await;
        let value: Value = serde_json::from_str(&result.content).expect("structured json");

        assert!(!result.is_error);
        assert_eq!(value["kind"], "aws_recon_report");
        assert_eq!(value["mode"], "fixture");
        assert_eq!(value["artifact_dir"], "/tmp/mu-fixture");
        assert_eq!(value["summary_path"], "/tmp/mu-fixture/summary.json");
        assert_eq!(value["capability"], "aws.scout.readonly");
        assert_eq!(value["call_timeout_secs"], 60);
        assert_eq!(value["audit"]["catalog_digest"], "sha256:test");
        assert_eq!(value["audit"]["aws_profile"], "mu-readonly-scout");
    }

    #[tokio::test]
    async fn fixture_unset_returns_runner_not_configured_refusal() {
        let tool = AwsReconTool::new(readonly_catalog(), "sha256:test", None);
        let result = execute(&tool, json!({})).await;
        let value: Value = serde_json::from_str(&result.content).expect("structured json");

        assert!(result.is_error);
        assert_eq!(value["reason"], "runner_not_configured");
        assert_eq!(
            value["audit"]["role_arn"],
            "arn:aws:iam::123456789012:role/mu-readonly-scout"
        );
    }

    #[tokio::test]
    async fn runner_mode_invokes_capability_runner_and_returns_report_shape() {
        let dir = temp_test_dir("aws-recon-runner-ok");
        let runner = dir.join("runner.sh");
        let script = dir.join("aws-recon.py");
        write_executable(
            &runner,
            r#"#!/bin/sh
printf 'capability=%s\n' "$1" >&2
shift
[ "$1" = "--" ] || exit 2
shift
exec "$@"
"#,
        );
        write_executable(
            &script,
            r#"#!/bin/sh
printf 'reports/aws-recon/20260514T000000Z\n'
printf '{"account":"123456789012","aws_profile":"mu-readonly-scout","capability_used":"aws.scout.readonly","report":"reports/aws-recon/20260514T000000Z","errors":[],"findings":[]}\n'
"#,
        );

        let tool = AwsReconTool::with_runner(
            readonly_catalog(),
            "sha256:test",
            &runner,
            &script,
            Some(dir.clone()),
        );
        let result = execute(
            &tool,
            json!({"call_timeout_secs": 60, "output_dir": "reports/aws-recon"}),
        )
        .await;
        let value: Value = serde_json::from_str(&result.content).expect("structured json");

        assert!(!result.is_error);
        assert_eq!(value["kind"], "aws_recon_report");
        assert_eq!(value["mode"], "runner");
        assert_eq!(value["report_dir"], "reports/aws-recon/20260514T000000Z");
        assert_eq!(
            value["summary_path"],
            "reports/aws-recon/20260514T000000Z/summary.json"
        );
        assert_eq!(value["capability"], "aws.scout.readonly");
        assert_eq!(value["call_timeout_secs"], 60);
        assert_eq!(
            value["runner_timeout_secs"],
            default_runner_timeout_secs(60)
        );
        assert_eq!(value["audit"]["aws_profile"], "mu-readonly-scout");
        assert_eq!(value["stdout_summary"]["errors"], json!([]));
        assert_eq!(value["output_capture"]["stdout_truncated"], false);
    }

    #[tokio::test]
    async fn runner_timeout_kills_hung_runner() {
        let dir = temp_test_dir("aws-recon-runner-timeout");
        let runner = dir.join("runner.sh");
        let script = dir.join("aws-recon.py");
        write_executable(
            &runner,
            r#"#!/bin/sh
shift
shift
exec "$@"
"#,
        );
        write_executable(
            &script,
            r#"#!/bin/sh
sleep 20
"#,
        );

        let tool = AwsReconTool::with_runner(
            readonly_catalog(),
            "sha256:test",
            &runner,
            &script,
            Some(dir),
        );
        let result = execute(&tool, json!({"runner_timeout_secs": 1})).await;
        let value: Value = serde_json::from_str(&result.content).expect("structured json");

        assert!(result.is_error);
        assert_eq!(value["reason"], "runner_timeout");
        assert!(value["message"].as_str().expect("message").contains("1s"));
    }

    #[test]
    fn runner_summary_missing_required_fields_is_refused() {
        let err = parse_runner_summary(r#"{"report":"reports/aws-recon/x","errors":[]}"#)
            .expect_err("missing findings must fail");

        assert!(err.contains("findings"));
    }

    #[tokio::test]
    async fn large_runner_stdout_is_bounded_and_flagged() {
        let (reader, mut writer) = tokio::io::duplex(8192);
        let writer_task = tokio::spawn(async move {
            let chunk = vec![b'x'; 8192];
            let mut remaining = MAX_RUNNER_STREAM_BYTES + 1;
            while remaining > 0 {
                let n = remaining.min(chunk.len());
                writer.write_all(&chunk[..n]).await.expect("write chunk");
                remaining -= n;
            }
            writer.shutdown().await.expect("shutdown writer");
        });

        let capture = read_limited(Some(reader)).await;
        writer_task.await.expect("writer task joins");

        assert!(capture.truncated);
        assert_eq!(capture.text.len(), MAX_RUNNER_STREAM_BYTES);
    }

    #[tokio::test]
    async fn runner_failure_is_structured_error() {
        let dir = temp_test_dir("aws-recon-runner-fail");
        let runner = dir.join("runner.sh");
        let script = dir.join("aws-recon.py");
        write_executable(
            &runner,
            r#"#!/bin/sh
printf 'runner refused\n' >&2
exit 42
"#,
        );
        write_executable(&script, "#!/bin/sh\nexit 0\n");

        let tool = AwsReconTool::with_runner(
            readonly_catalog(),
            "sha256:test",
            &runner,
            &script,
            Some(dir),
        );
        let result = execute(&tool, json!({})).await;
        let value: Value = serde_json::from_str(&result.content).expect("structured json");

        assert!(result.is_error);
        assert_eq!(value["reason"], "runner_failed");
        assert!(value["message"]
            .as_str()
            .expect("message")
            .contains("runner refused"));
    }

    #[tokio::test]
    async fn unsupported_capability_is_refused_before_catalog_lookup() {
        let tool = AwsReconTool::new(readonly_catalog(), "sha256:test", Some("fixture".into()));
        let result = execute(&tool, json!({"capability": "aws.audit.security"})).await;
        let value: Value = serde_json::from_str(&result.content).expect("structured json");

        assert!(result.is_error);
        assert_eq!(value["reason"], "unsupported_capability");
        assert_eq!(value["audit"]["capability"], "aws.audit.security");
    }

    /// Root of an exec-allowed test tempdir. `std::env::temp_dir()` is `/tmp`
    /// on most systems, which is mounted `noexec` on FreeBSD (and on hardened
    /// Linux configs). aws_recon tests write shell scripts and exec them, so
    /// we route through the workspace `target/` directory instead — that's
    /// exec-allowed by construction (cargo builds binaries there) and
    /// `cargo clean` handles cleanup.
    fn exec_temp_root() -> PathBuf {
        // `CARGO_TARGET_TMPDIR` is set by cargo for integration tests in `tests/`.
        if let Some(p) = std::env::var_os("CARGO_TARGET_TMPDIR") {
            return PathBuf::from(p);
        }
        // Unit tests get `CARGO_MANIFEST_DIR` (crate root). Walk up looking
        // for the workspace `target/` directory.
        if let Some(m) = std::env::var_os("CARGO_MANIFEST_DIR") {
            let mut path = PathBuf::from(m);
            loop {
                let target = path.join("target");
                if target.is_dir() {
                    return target.join("test-tmp");
                }
                if !path.pop() {
                    break;
                }
            }
        }
        // Last resort: env::temp_dir(). Tests that exec scripts will fail
        // here if /tmp is noexec — but that's strictly better than today.
        std::env::temp_dir()
    }

    fn temp_test_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let dir = exec_temp_root().join(format!("mu-{name}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_executable(path: &std::path::Path, content: &str) {
        fs::write(path, content).expect("write script");
        let mut perms = fs::metadata(path).expect("script metadata").permissions();
        perms.set_mode(0o700);
        fs::set_permissions(path, perms).expect("chmod script");
    }
}
