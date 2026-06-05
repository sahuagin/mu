use std::collections::BTreeMap;

use axum::response::Html;
use mu_core::{agent::Usage, event_log::EventPayload};

use crate::console::data::AppState;

pub(crate) fn page(state: &AppState, title: &str, body: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><html><head><meta charset=utf-8><title>{}</title>{}</head><body><header><a href=\"{}\">μ console</a></header><main>{}</main></body></html>",
        esc(title),
        STYLE,
        esc_attr(&state.href("/sessions")),
        body
    ))
}

pub(crate) fn block(out: &mut String, who: &str, text: &str) {
    out.push_str(&format!(
        "<section class=block><h3>{}</h3><pre>{}</pre></section>",
        esc(who),
        esc(&truncate(text, 60_000))
    ));
}

pub(crate) fn breakdown_table(map: &BTreeMap<String, u64>) -> String {
    if map.is_empty() {
        return "<p class=muted>No token breakdown recorded.</p>".into();
    }
    let mut out = String::from("<h3>token breakdown</h3><table><thead><tr><th>section</th><th>tokens</th></tr></thead><tbody>");
    for (k, v) in map {
        out.push_str("<tr>");
        out.push_str(&td(k));
        out.push_str(&td_num(*v));
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table>");
    out
}

pub(crate) fn kv(out: &mut String, k: &str, v: &str) {
    out.push_str(&format!("<dt>{}</dt><dd>{}</dd>", esc(k), esc(v)));
}

pub(crate) fn td(s: &str) -> String {
    format!("<td>{}</td>", esc(s))
}

pub(crate) fn td_code(s: &str) -> String {
    format!("<td><code>{}</code></td>", esc(s))
}

pub(crate) fn td_num(n: impl std::fmt::Display) -> String {
    format!("<td class=num>{n}</td>")
}

pub(crate) fn payload_kind(payload: &EventPayload) -> &'static str {
    match payload {
        EventPayload::SessionCreated { .. } => "session_created",
        EventPayload::UserMessage { .. } => "user_message",
        EventPayload::AssistantMessageEvent { .. } => "assistant_message_event",
        EventPayload::ToolCall { .. } => "tool_call",
        EventPayload::ToolResult { .. } => "tool_result",
        EventPayload::Done { .. } => "done",
        EventPayload::Error { .. } => "error",
        EventPayload::Callout { .. } => "callout",
        EventPayload::ErrorInvalidMessage { .. } => "error_invalid_message",
        EventPayload::ProviderSwitched { .. } => "provider_switched",
        EventPayload::SessionClosed => "session_closed",
        EventPayload::ContextAssembly { .. } => "context_assembly",
        EventPayload::CompactionAssembly { .. } => "compaction_assembly",
        EventPayload::AutonomousIterationStarted { .. } => "autonomous_iteration_started",
        EventPayload::AutonomousIterationCompleted { .. } => "autonomous_iteration_completed",
        EventPayload::AutonomousScheduledWakeup { .. } => "autonomous_scheduled_wakeup",
        EventPayload::AutonomousTerminated { .. } => "autonomous_terminated",
        EventPayload::ProviderStatusUpdate { .. } => "provider_status_update",
        EventPayload::MailboxMessagePosted { .. } => "mailbox_message_posted",
        EventPayload::MailboxMessageConsumed { .. } => "mailbox_message_consumed",
        EventPayload::TaskTelemetry { .. } => "task_telemetry",
        EventPayload::WorkerSpawned { .. } => "worker_spawned",
        EventPayload::WorkerExited { .. } => "worker_exited",
        EventPayload::WorkerFailed { .. } => "worker_failed",
        EventPayload::WorkerTimeout { .. } => "worker_timeout",
    }
}

pub(crate) fn fmt_usage_short(u: Option<Usage>) -> String {
    match u {
        Some(u) => format!(
            "in {} / out {} / read {} / write {}",
            u.input_tokens,
            u.output_tokens,
            fmt_opt_u64(u.cache_read_input_tokens),
            fmt_opt_u64(u.cache_creation_input_tokens)
        ),
        None => "—".into(),
    }
}

pub(crate) fn fmt_opt_u64(v: Option<u64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "—".into())
}

pub(crate) fn fmt_opt_u32(v: Option<u32>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "—".into())
}

pub(crate) fn fmt_ms(v: Option<u64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "—".into())
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}\n… truncated {} byte(s)", &s[..max], s.len() - max)
    }
}

pub(crate) fn urlish(s: &str) -> String {
    s.to_string()
}

pub(crate) fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub(crate) fn esc_attr(s: &str) -> String {
    esc(s)
}

const STYLE: &str = r#"<style>
:root { color-scheme: dark light; --bg:#0f1117; --fg:#e6edf3; --muted:#8b949e; --line:#30363d; --link:#79c0ff; --warn:#f2cc60; --err:#ff7b72; }
body { margin:0; font:14px/1.45 system-ui, sans-serif; background:var(--bg); color:var(--fg); }
header { padding:10px 18px; border-bottom:1px solid var(--line); position:sticky; top:0; background:var(--bg); }
main { padding:18px; max-width:1400px; }
a { color:var(--link); text-decoration:none; } a:hover { text-decoration:underline; }
table { border-collapse:collapse; width:100%; margin:12px 0 24px; font-size:13px; }
th, td { border:1px solid var(--line); padding:6px 8px; vertical-align:top; }
th { text-align:left; background:#161b22; position:sticky; top:42px; }
.num { text-align:right; font-variant-numeric:tabular-nums; }
code, pre { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
pre { white-space:pre-wrap; overflow:auto; background:#161b22; border:1px solid var(--line); padding:10px; border-radius:6px; }
.block { border:1px solid var(--line); border-radius:8px; padding:0 12px 12px; margin:12px 0; }
.block h3 { color:var(--muted); }
.muted { color:var(--muted); } .warn { color:var(--warn); } .err { color:var(--err); }
.tabs { display:flex; gap:8px; margin:12px 0 18px; flex-wrap:wrap; }
.tabs a { border:1px solid var(--line); padding:5px 9px; border-radius:999px; background:#161b22; }
.kv { display:grid; grid-template-columns:max-content 1fr; gap:6px 14px; }
.kv dt { color:var(--muted); } .kv dd { margin:0; }
</style>"#;
