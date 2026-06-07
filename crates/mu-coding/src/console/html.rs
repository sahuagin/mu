use std::collections::BTreeMap;

use axum::response::Html;
use mu_core::event_log::EventPayload;

use crate::console::data::AppState;
use crate::console::time::civil_from_days;

pub(crate) fn page(state: &AppState, title: &str, body: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><html><head><meta charset=utf-8><title>{}</title>{}<script>{}</script></head><body><header><a href=\"{}\">μ console</a></header><main>{}</main></body></html>",
        esc(title),
        STYLE,
        SCRIPT,
        esc_attr(&state.href("/sessions")),
        body
    ))
}

pub(crate) fn transcript_block(
    out: &mut String,
    event_id: u64,
    timestamp_unix_ms: u64,
    role: &str,
    text: &str,
    open: bool,
) {
    let classes = format!("block role-{role}");
    let ts = fmt_unix_ms(Some(timestamp_unix_ms));
    let open_attr = if open { " open" } else { "" };
    out.push_str(&format!(
        "<section id=\"event-{event_id}\" class=\"{}\"><details{}><summary><span class=role>{}</span> <a class=anchor href=\"#event-{}\">#{}</a> <span class=muted>{}</span></summary><pre class=scrollbox>{}</pre></details></section>",
        esc_attr(&classes),
        open_attr,
        esc(role),
        event_id,
        event_id,
        esc(&ts),
        esc(&truncate(text, 60_000))
    ));
}

#[allow(dead_code)]
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

pub(crate) fn td_time(v: Option<u64>) -> String {
    format!("<td>{}</td>", fmt_unix_ms(v))
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
        EventPayload::OperatorMark { .. } => "operator_mark",
    }
}

pub(crate) fn fmt_opt_u64(v: Option<u64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "—".into())
}

pub(crate) fn fmt_opt_u32(v: Option<u32>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "—".into())
}

pub(crate) fn fmt_unix_ms(v: Option<u64>) -> String {
    v.map(time_tag).unwrap_or_else(|| "—".into())
}

pub(crate) fn time_tag(ms: u64) -> String {
    format!(
        "<time datetime=\"{}\" data-epoch-ms=\"{}\">{}.{:03}s</time>",
        esc_attr(&epoch_ms_to_iso_utc(ms)),
        ms,
        ms / 1000,
        ms % 1000
    )
}

fn epoch_ms_to_iso_utc(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let millis = ms % 1000;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = sod / 3600;
    let minute = (sod % 3600) / 60;
    let second = sod % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

pub(crate) fn truncated_details(label: &str, text: &str, max: usize) -> String {
    if text.len() <= max {
        return format!("<pre>{}</pre>", esc(text));
    }
    format!(
        "<details><summary>{} — showing first {} of {} byte(s)</summary><pre>{}</pre></details>",
        esc(label),
        max,
        text.len(),
        esc(&truncate(text, max))
    )
}

pub(crate) fn event_anchor(event_id: u64) -> String {
    format!(
        "<a class=anchor href=\"#event-{}\">#{}</a>",
        event_id, event_id
    )
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

const SCRIPT: &str = r#"
function toggleRole(role, checked) {
  document.querySelectorAll('.role-' + role).forEach(el => {
    el.style.display = checked ? '' : 'none';
  });
}
function expandAll(selector) {
  document.querySelectorAll(selector + ' details').forEach(el => el.open = true);
}
function collapseAll(selector) {
  document.querySelectorAll(selector + ' details').forEach(el => el.open = false);
}
function setTranscriptBodyScroll(enabled) {
  document.querySelectorAll('#transcript pre').forEach(el => {
    if (enabled) {
      el.classList.add('scrollbox');
    } else {
      el.classList.remove('scrollbox');
    }
  });
}
function localizeTimes() {
  const times = Array.from(document.querySelectorAll('time[data-epoch-ms]'));
  if (times.length === 0) return;
  const first = Number(times[0].dataset.epochMs);
  times.forEach(el => {
    const ms = Number(el.dataset.epochMs);
    if (!Number.isFinite(ms)) return;
    const date = new Date(ms);
    const elapsed = ms >= first ? '+' + formatElapsed(ms - first) : '';
    const absolute = date.toLocaleString(undefined, {
      year: 'numeric', month: '2-digit', day: '2-digit',
      hour: '2-digit', minute: '2-digit', second: '2-digit',
      fractionalSecondDigits: 3,
      hour12: false,
    });
    el.textContent = elapsed ? absolute + ' · ' + elapsed : absolute;
    el.title = date.toISOString();
  });
}
function formatElapsed(deltaMs) {
  const ms = deltaMs % 1000;
  let seconds = Math.floor(deltaMs / 1000);
  const hours = Math.floor(seconds / 3600);
  seconds -= hours * 3600;
  const minutes = Math.floor(seconds / 60);
  seconds -= minutes * 60;
  if (hours > 0) return hours + 'h ' + minutes + 'm ' + seconds + '.' + String(ms).padStart(3, '0') + 's';
  if (minutes > 0) return minutes + 'm ' + seconds + '.' + String(ms).padStart(3, '0') + 's';
  return seconds + '.' + String(ms).padStart(3, '0') + 's';
}
"#;

const STYLE: &str = r#"<style>
:root { color-scheme: dark light; --bg:#0f1117; --fg:#e6edf3; --muted:#8b949e; --line:#30363d; --link:#79c0ff; --warn:#f2cc60; --err:#ff7b72; }
body { margin:0; font:14px/1.45 system-ui, sans-serif; background:var(--bg); color:var(--fg); }
:target { outline:2px solid var(--link); outline-offset:3px; }
header { padding:10px 18px; border-bottom:1px solid var(--line); position:sticky; top:0; background:var(--bg); z-index:10; }
main { padding:18px; max-width:1400px; }
a { color:var(--link); text-decoration:none; } a:hover { text-decoration:underline; }
.anchor { color:var(--muted); font-size:12px; }
.toolbar { display:flex; gap:12px; align-items:center; flex-wrap:wrap; margin:10px 0 16px; padding:8px 10px; border:1px solid var(--line); border-radius:8px; background:#161b22; position:sticky; top:44px; z-index:9; }
.toolbar button { color:var(--fg); background:#21262d; border:1px solid var(--line); border-radius:6px; padding:3px 8px; }
table { border-collapse:collapse; width:100%; margin:12px 0 24px; font-size:13px; }
th, td { border:1px solid var(--line); padding:6px 8px; vertical-align:top; }
th { text-align:left; background:#161b22; position:sticky; top:42px; }
.num { text-align:right; font-variant-numeric:tabular-nums; }
code, pre { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
pre { white-space:pre-wrap; overflow:auto; background:#161b22; border:1px solid var(--line); padding:10px; border-radius:6px; }
pre.scrollbox { max-height:65vh; }
summary { cursor:pointer; color:var(--link); }
.block { border:1px solid var(--line); border-radius:8px; padding:0 12px 12px; margin:12px 0; }
.block details { padding:10px 0; }
.block .role { text-transform:uppercase; letter-spacing:.05em; color:var(--muted); font-size:12px; }
.role-user { border-left:3px solid #58a6ff; }
.role-assistant { border-left:3px solid #a5d6ff; }
.role-tool { border-left:3px solid #d29922; }
.block h3 { color:var(--muted); }
.muted { color:var(--muted); } .warn { color:var(--warn); } .err { color:var(--err); }
.tabs { display:flex; gap:8px; margin:12px 0 18px; flex-wrap:wrap; }
.tabs a { border:1px solid var(--line); padding:5px 9px; border-radius:999px; background:#161b22; }
.kv { display:grid; grid-template-columns:max-content 1fr; gap:6px 14px; }
.kv dt { color:var(--muted); } .kv dd { margin:0; }
</style>"#;
