use std::sync::Arc;

use axum::response::Html;
use mu_core::{
    agent::{ContentBlock, Usage},
    event_log::{EventPayload, SessionEvent},
};

use crate::console::{
    cc_data::{read_cc_transcript, CcRole, CcTranscript},
    data::{load_events, scan_all, AppState},
    html::{
        breakdown_table, esc, esc_attr, event_anchor, fmt_opt_u32, fmt_opt_u64, kv, page,
        payload_kind, td, td_code, td_num, td_time, transcript_block, truncate, truncated_details,
        urlish,
    },
};

#[derive(Debug, Clone, Copy)]
pub(crate) enum DetailTab {
    Overview,
    Events,
    Cost,
    Context,
    Compactions,
}

/// mu-cc-sessions-console-lqqt.2: tabs for the claude-code detail view.
/// cc transcripts have no ContextAssembly/Compaction events, so the cc
/// view carries only the transcript, a raw-JSON drilldown, and a cost
/// tab fed by per-assistant-turn `message.usage`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CcDetailTab {
    Transcript,
    Events,
    Cost,
}

pub(crate) fn render_sessions_index(state: Arc<AppState>) -> Html<String> {
    let scan = scan_all(
        &state.events_dir,
        state.cc_projects_dir.as_deref(),
        state.cc_marks_db.as_deref(),
    );
    let mut body = String::new();
    body.push_str("<h1>mu sessions</h1>");
    body.push_str(&format!(
        "<p class=muted>events: <code>{}</code></p>",
        esc(&state.events_dir.display().to_string())
    ));
    if let Some(db) = &state.analytics_db {
        body.push_str(&format!(
            "<p class=muted>analytics: <code>{}</code></p>",
            esc(&db.display().to_string())
        ));
    }
    // mu-cc-sessions-console-lqqt.1: signal when claude-code sessions
    // are merged into the index so the merged corpus isn't a surprise.
    if let Some(cc) = &state.cc_projects_dir {
        body.push_str(&format!(
            "<p class=muted>claude-code: <code>{}</code></p>",
            esc(&cc.display().to_string())
        ));
    }
    if scan.malformed_files > 0 || scan.skipped_entries > 0 {
        body.push_str(&format!(
            "<p class=warn>Skipped {} entrie(s), {} malformed file(s).</p>",
            scan.skipped_entries, scan.malformed_files
        ));
    }
    // mu-y5hz: subagent (sidechain) turns are excluded from per-session
    // rollups; surface the corpus total so the exclusion is visible and
    // not a silent drop. One muted line, only when non-zero (no column).
    let sidechain_total: u64 = scan
        .sessions
        .iter()
        .map(|s| u64::from(s.sidechain_entries))
        .sum();
    if sidechain_total > 0 {
        body.push_str(&format!(
            "<p class=muted>Excluded {sidechain_total} subagent (sidechain) turn(s) from session rollups.</p>"
        ));
    }
    body.push_str("<table><thead><tr><th>last</th><th>daemon</th><th>session</th><th>mark</th><th>provider</th><th>model</th><th>asks</th><th>calls</th><th>tools</th><th>input</th><th>output</th><th>cache read</th><th>cache write</th></tr></thead><tbody>");
    for s in scan.sessions {
        // mu-cc-sessions-console-lqqt.2: claude-code rows open the cc
        // detail route (a separate reader over the cc projects dir);
        // native mu rows keep the event-log detail route.
        let is_cc = s.provider.as_deref() == Some("claude-code");
        let detail_root = if is_cc { "/cc" } else { "/sessions" };
        let href = state.href(&format!(
            "{}/{}/{}",
            detail_root,
            urlish(&s.daemon_id),
            urlish(&s.session_id)
        ));
        body.push_str("<tr>");
        body.push_str(&td_time(s.last_activity_unix_ms));
        body.push_str(&td_code(&s.daemon_id));
        body.push_str(&format!(
            "<td><a href=\"{}\"><code>{}</code></a></td>",
            esc_attr(&href),
            esc(&s.session_id)
        ));
        // mu-index-mark-column-auiv: coverage at a glance — which
        // sessions already carry an operator mark.
        body.push_str(&td(&s.mark.map(stars).unwrap_or_else(|| "—".into())));
        body.push_str(&td(&s.provider.unwrap_or_else(|| "—".into())));
        // mu-y5hz: the model column is last-model-wins; annotate inline
        // (no extra column) when a session switched models mid-run so the
        // earlier model isn't invisible.
        let model_cell = match s.model {
            Some(m) if s.models_seen > 1 => {
                format!(
                    "{} <span class=muted>({} models)</span>",
                    esc(&m),
                    s.models_seen
                )
            }
            Some(m) => esc(&m),
            None => "—".into(),
        };
        body.push_str(&format!("<td>{model_cell}</td>"));
        body.push_str(&td_num(s.ask_count));
        body.push_str(&td_num(s.context_assembly_count));
        body.push_str(&td_num(s.tool_call_count));
        if let Some(usage) = s.usage {
            body.push_str(&td_num(usage.input_tokens));
            body.push_str(&td_num(usage.output_tokens));
            body.push_str(&td(&fmt_opt_u64(usage.cache_read_input_tokens)));
            body.push_str(&td(&fmt_opt_u64(usage.cache_creation_input_tokens)));
        } else {
            body.push_str(&td("—"));
            body.push_str(&td("—"));
            body.push_str(&td("—"));
            body.push_str(&td("—"));
        }
        body.push_str("</tr>");
    }
    body.push_str("</tbody></table>");
    page(&state, "sessions", &body)
}

pub(crate) fn render_session_page(
    state: Arc<AppState>,
    daemon_id: String,
    session_id: String,
    tab: DetailTab,
) -> Html<String> {
    match load_events(&state.events_dir, &daemon_id, &session_id) {
        Ok((events, malformed)) => {
            let mut body = session_header(&state, &daemon_id, &session_id, &events, malformed);
            body.push_str(&session_nav(&state, &daemon_id, &session_id));
            match tab {
                DetailTab::Overview => body.push_str(&render_transcript(&events)),
                DetailTab::Events => body.push_str(&render_events(&events)),
                DetailTab::Cost => body.push_str(&render_cost(&events)),
                DetailTab::Context => body.push_str(&render_context_list(
                    &state,
                    &daemon_id,
                    &session_id,
                    &events,
                )),
                DetailTab::Compactions => {
                    body.push_str(&render_compaction_list(
                        &state,
                        &daemon_id,
                        &session_id,
                        &events,
                    ));
                }
            }
            page(&state, &session_id, &body)
        }
        Err(e) => page(
            &state,
            "session not found",
            &format!(
                "<h1>session not found</h1><p class=err>{}</p>",
                esc(&e.to_string())
            ),
        ),
    }
}

pub(crate) fn render_context_one(
    state: Arc<AppState>,
    daemon_id: String,
    session_id: String,
    model_call_id: u32,
) -> Html<String> {
    let Ok((events, malformed)) = load_events(&state.events_dir, &daemon_id, &session_id) else {
        return page(&state, "context", "<h1>session not found</h1>");
    };
    let mut body = session_header(&state, &daemon_id, &session_id, &events, malformed);
    body.push_str(&session_nav(&state, &daemon_id, &session_id));
    body.push_str(&format!("<h2>ContextAssembly #{model_call_id}</h2>"));
    for ev in &events {
        if let EventPayload::ContextAssembly {
            model_call_id: id,
            message_count,
            user_message_count,
            assistant_message_count,
            tool_result_count,
            tool_count,
            token_count_estimate,
            token_breakdown,
            provider_kind,
            model,
            renderer,
            cache_strategy,
            span_count,
            cache_boundary_count,
            first_span_ids,
            prefix_hash,
            prefix_span_hashes,
        } = &ev.payload
        {
            if *id == model_call_id {
                body.push_str("<dl class=kv>");
                kv(&mut body, "event", &ev.id.to_string());
                kv(&mut body, "provider", provider_kind);
                kv(&mut body, "model", model);
                kv(&mut body, "renderer", renderer.as_deref().unwrap_or("—"));
                kv(
                    &mut body,
                    "cache strategy",
                    cache_strategy.as_deref().unwrap_or("—"),
                );
                kv(
                    &mut body,
                    "token estimate",
                    &fmt_opt_u64(*token_count_estimate),
                );
                kv(&mut body, "messages", &message_count.to_string());
                kv(
                    &mut body,
                    "user/assistant/tool_result",
                    &format!("{user_message_count}/{assistant_message_count}/{tool_result_count}"),
                );
                kv(&mut body, "tools", &tool_count.to_string());
                kv(&mut body, "spans", &fmt_opt_u32(*span_count));
                kv(
                    &mut body,
                    "cache boundaries",
                    &fmt_opt_u32(*cache_boundary_count),
                );
                kv(
                    &mut body,
                    "prefix hash",
                    prefix_hash.as_deref().unwrap_or("—"),
                );
                kv(&mut body, "first span ids", &first_span_ids.join(", "));
                body.push_str("</dl>");
                body.push_str(&breakdown_table(token_breakdown));
                if !prefix_span_hashes.is_empty() {
                    body.push_str("<h3>prefix span hashes</h3><pre>");
                    body.push_str(&esc(&prefix_span_hashes.join("\n")));
                    body.push_str("</pre>");
                }
                return page(&state, "context", &body);
            }
        }
    }
    body.push_str("<p class=warn>No matching ContextAssembly.</p>");
    page(&state, "context", &body)
}

pub(crate) fn render_compaction_one(
    state: Arc<AppState>,
    daemon_id: String,
    session_id: String,
    model_call_id: u32,
) -> Html<String> {
    let Ok((events, malformed)) = load_events(&state.events_dir, &daemon_id, &session_id) else {
        return page(&state, "compaction", "<h1>session not found</h1>");
    };
    let mut body = session_header(&state, &daemon_id, &session_id, &events, malformed);
    body.push_str(&session_nav(&state, &daemon_id, &session_id));
    body.push_str(&format!("<h2>CompactionAssembly #{model_call_id}</h2>"));
    for ev in &events {
        if let EventPayload::CompactionAssembly {
            model_call_id: id,
            policy_id,
            tokens_before,
            tokens_after,
            decisions,
            wall_clock_us,
        } = &ev.payload
        {
            if *id == model_call_id {
                body.push_str("<dl class=kv>");
                kv(&mut body, "event", &ev.id.to_string());
                kv(&mut body, "policy", policy_id);
                kv(&mut body, "tokens before", &tokens_before.to_string());
                kv(&mut body, "tokens after", &tokens_after.to_string());
                kv(&mut body, "wall clock us", &wall_clock_us.to_string());
                kv(&mut body, "decisions", &decisions.len().to_string());
                body.push_str("</dl><h3>decisions</h3><pre>");
                body.push_str(&esc(&truncate(&format!("{decisions:#?}"), 80_000)));
                body.push_str("</pre>");
                return page(&state, "compaction", &body);
            }
        }
    }
    body.push_str("<p class=warn>No matching CompactionAssembly.</p>");
    page(&state, "compaction", &body)
}

pub(crate) fn compare_placeholder(
    state: Arc<AppState>,
    left: Option<String>,
    right: Option<String>,
) -> Html<String> {
    let body = format!(
        "<h1>compare</h1><p>Comparison UI is planned after session/cost/context views.</p><p>left={}; right={}</p>",
        esc(&left.unwrap_or_else(|| "—".into())),
        esc(&right.unwrap_or_else(|| "—".into()))
    );
    page(&state, "compare", &body)
}

fn session_header(
    state: &AppState,
    daemon_id: &str,
    session_id: &str,
    events: &[SessionEvent],
    malformed: usize,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "<p><a href=\"{}\">← sessions</a></p>",
        esc_attr(&state.href("/sessions"))
    ));
    out.push_str(&format!(
        "<h1><code>{}</code></h1><p class=muted>daemon <code>{}</code> · {} event(s)</p>",
        esc(session_id),
        esc(daemon_id),
        events.len()
    ));
    if malformed > 0 {
        out.push_str(&format!(
            "<p class=warn>{malformed} malformed line(s) skipped while reading this log.</p>"
        ));
    }
    out.push_str(&mark_line(state, daemon_id, session_id, events));
    out
}

/// mu-operator-mark-5mwr: current operator mark (latest `OperatorMark`
/// by event id wins) plus the inline re-mark form. The form POSTs to
/// the console's one write route; the page re-render then shows
/// whatever the log says.
/// mu-index-mark-column-auiv: `★★☆☆☆`-style rendering of a 1-5 rating,
/// shared by the sessions index column and the session-header line.
fn stars(rating: u8) -> String {
    let filled = usize::from(rating).min(5);
    format!("{}{}", "★".repeat(filled), "☆".repeat(5 - filled))
}

fn mark_line(
    state: &AppState,
    daemon_id: &str,
    session_id: &str,
    events: &[SessionEvent],
) -> String {
    let current = events.iter().rev().find_map(|ev| match &ev.payload {
        EventPayload::OperatorMark { rating, note } => Some((*rating, note.clone())),
        _ => None,
    });
    let mut out = String::from("<p class=muted>mark: ");
    match &current {
        Some((rating, note)) => {
            out.push_str(&stars(*rating));
            out.push_str(&format!(" {rating}/5"));
            if let Some(note) = note {
                out.push_str(&format!(" — {}", esc(note)));
            }
        }
        None => out.push_str("unmarked"),
    }
    out.push_str("</p>");
    let action = state.href(&format!(
        "/sessions/{}/{}/mark",
        urlish(daemon_id),
        urlish(session_id)
    ));
    let selected = current.as_ref().map(|(r, _)| *r);
    let mut options = String::new();
    for (value, label) in [
        (1u8, "1 — unusable"),
        (2, "2 — poor"),
        (3, "3 — ok"),
        (4, "4 — good"),
        (5, "5 — excellent"),
    ] {
        options.push_str(&format!(
            "<option value={value}{}>{label}</option>",
            if selected == Some(value) {
                " selected"
            } else {
                ""
            }
        ));
    }
    // mu-mark-note-prefill-gjm6: carry the current note into the form
    // so a re-rank doesn't silently clear it — each OperatorMark event
    // stays complete, the operator just doesn't retype.
    let note_value = current
        .and_then(|(_, note)| note)
        .map(|n| format!(" value=\"{}\"", esc_attr(&n)))
        .unwrap_or_default();
    out.push_str(&format!(
        "<form class=toolbar method=post action=\"{}\">\
           <label>rate <select name=rating>{options}</select></label> \
           <input type=text name=note placeholder=\"optional note\"{note_value} size=40 maxlength=400> \
           <button type=submit>mark</button>\
         </form>",
        esc_attr(&action)
    ));
    out
}

fn session_nav(state: &AppState, daemon_id: &str, session_id: &str) -> String {
    let root = format!("/sessions/{}/{}", urlish(daemon_id), urlish(session_id));
    let items = [
        ("overview", root.clone()),
        ("events", format!("{root}/events")),
        ("cost", format!("{root}/cost")),
        ("context", format!("{root}/context")),
        ("compactions", format!("{root}/compactions")),
    ];
    let mut out = String::from("<nav class=tabs>");
    for (label, path) in items {
        out.push_str(&format!(
            "<a href=\"{}\">{}</a>",
            esc_attr(&state.href(&path)),
            esc(label)
        ));
    }
    out.push_str("</nav>");
    out
}

fn render_transcript(events: &[SessionEvent]) -> String {
    let mut out = String::from(
        "<h2>transcript</h2>\
         <div class=toolbar>\
           <label><input type=checkbox checked onchange=\"toggleRole('user', this.checked)\"> user</label>\
           <label><input type=checkbox checked onchange=\"toggleRole('assistant', this.checked)\"> assistant</label>\
           <label><input type=checkbox checked onchange=\"toggleRole('tool', this.checked)\"> tools</label>\
           <button type=button onclick=\"expandAll('#transcript')\">expand all</button>\
           <button type=button onclick=\"collapseAll('#transcript')\">collapse all</button>\
           <label><input type=checkbox checked onchange=\"setTranscriptBodyScroll(this.checked)\"> scroll bodies</label>\
         </div><div id=transcript>",
    );
    for ev in events {
        match &ev.payload {
            EventPayload::UserMessage { content } => {
                transcript_block(
                    &mut out,
                    ev.id,
                    ev.timestamp_unix_ms,
                    "user",
                    content,
                    false,
                );
            }
            EventPayload::AssistantMessageEvent { message } => {
                let mut text = String::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text: t } => {
                            text.push_str(t);
                            text.push('\n');
                        }
                        ContentBlock::Thinking { text: t } => {
                            text.push_str("[thinking] ");
                            text.push_str(t);
                            text.push('\n');
                        }
                        ContentBlock::ToolCall(call) => {
                            text.push_str(&format!(
                                "[tool_call {} {}] {}\n",
                                call.id,
                                call.name,
                                call.arguments.as_value()
                            ));
                        }
                    }
                }
                transcript_block(
                    &mut out,
                    ev.id,
                    ev.timestamp_unix_ms,
                    "assistant",
                    &text,
                    false,
                );
            }
            EventPayload::ToolCall {
                call_id,
                name,
                arguments,
            } => {
                transcript_block(
                    &mut out,
                    ev.id,
                    ev.timestamp_unix_ms,
                    "tool",
                    &format!("{name} {call_id}\n{arguments}"),
                    true,
                );
            }
            EventPayload::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                transcript_block(
                    &mut out,
                    ev.id,
                    ev.timestamp_unix_ms,
                    "tool",
                    &format!("{call_id}\n{}", truncate(content, 60_000)),
                    !*is_error,
                );
            }
            _ => {}
        }
    }
    out.push_str("</div>");
    out
}

fn render_events(events: &[SessionEvent]) -> String {
    let mut out = String::from("<h2>event timeline</h2><div class=toolbar><button type=button onclick=\"expandAll('#events')\">expand json</button><button type=button onclick=\"collapseAll('#events')\">collapse json</button></div><table id=events><thead><tr><th>id</th><th>time</th><th>actor</th><th>kind</th><th>details</th></tr></thead><tbody>");
    for ev in events {
        out.push_str(&format!("<tr id=\"event-{}\">", ev.id));
        out.push_str(&format!("<td>{}</td>", event_anchor(ev.id)));
        out.push_str(&td_time(Some(ev.timestamp_unix_ms)));
        out.push_str(&td_code(&format!("{:?}", ev.actor)));
        out.push_str(&td_code(payload_kind(&ev.payload)));
        let json = serde_json::to_string_pretty(ev).unwrap_or_else(|_| format!("{ev:#?}"));
        out.push_str(&format!(
            "<td>{}</td>",
            truncated_details("json", &json, 40_000)
        ));
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table>");
    out
}

fn render_cost(events: &[SessionEvent]) -> String {
    let mut out = String::from("<h2>cost/cache</h2><p class=muted>V1 shows provider-reported usage buckets from events. Dollar pricing projection can layer on top of this view.</p>");
    let mut total = Usage::default();
    let mut any = false;
    out.push_str("<table><thead><tr><th>event</th><th>kind</th><th>input</th><th>output</th><th>cache read</th><th>cache write</th><th>5m write</th><th>1h write</th></tr></thead><tbody>");
    for ev in events {
        let usage = match &ev.payload {
            EventPayload::AssistantMessageEvent { message } => message.usage,
            EventPayload::Done { usage, .. } => *usage,
            _ => None,
        };
        if let Some(u) = usage {
            any = true;
            total = total + u;
            out.push_str("<tr>");
            out.push_str(&td_num(ev.id));
            out.push_str(&td_code(payload_kind(&ev.payload)));
            out.push_str(&td_num(u.input_tokens));
            out.push_str(&td_num(u.output_tokens));
            out.push_str(&td(&fmt_opt_u64(u.cache_read_input_tokens)));
            out.push_str(&td(&fmt_opt_u64(u.cache_creation_input_tokens)));
            out.push_str(&td(&fmt_opt_u64(u.cache_creation_5m_input_tokens)));
            out.push_str(&td(&fmt_opt_u64(u.cache_creation_1h_input_tokens)));
            out.push_str("</tr>");
        }
    }
    out.push_str("</tbody></table>");
    if any {
        out.push_str("<h3>summed buckets</h3><dl class=kv>");
        kv(&mut out, "input", &total.input_tokens.to_string());
        kv(&mut out, "output", &total.output_tokens.to_string());
        kv(
            &mut out,
            "cache read",
            &fmt_opt_u64(total.cache_read_input_tokens),
        );
        kv(
            &mut out,
            "cache write",
            &fmt_opt_u64(total.cache_creation_input_tokens),
        );
        kv(
            &mut out,
            "5m write",
            &fmt_opt_u64(total.cache_creation_5m_input_tokens),
        );
        kv(
            &mut out,
            "1h write",
            &fmt_opt_u64(total.cache_creation_1h_input_tokens),
        );
        out.push_str("</dl>");
    } else {
        out.push_str("<p class=warn>No usage records found in this session log.</p>");
    }
    out
}

fn render_context_list(
    state: &AppState,
    daemon_id: &str,
    session_id: &str,
    events: &[SessionEvent],
) -> String {
    let mut out = String::from("<h2>ContextAssembly</h2><table><thead><tr><th>call</th><th>event</th><th>tokens</th><th>spans</th><th>renderer</th><th>cache</th><th>prefix</th></tr></thead><tbody>");
    let mut n = 0;
    for ev in events {
        if let EventPayload::ContextAssembly {
            model_call_id,
            token_count_estimate,
            renderer,
            cache_strategy,
            span_count,
            prefix_hash,
            ..
        } = &ev.payload
        {
            n += 1;
            let href = state.href(&format!(
                "/sessions/{}/{}/context/{}",
                urlish(daemon_id),
                urlish(session_id),
                model_call_id
            ));
            out.push_str("<tr>");
            out.push_str(&format!(
                "<td><a href=\"{}\">{}</a></td>",
                esc_attr(&href),
                model_call_id
            ));
            out.push_str(&td_num(ev.id));
            out.push_str(&td(&fmt_opt_u64(*token_count_estimate)));
            out.push_str(&td(&fmt_opt_u32(*span_count)));
            out.push_str(&td(renderer.as_deref().unwrap_or("—")));
            out.push_str(&td(cache_strategy.as_deref().unwrap_or("—")));
            out.push_str(&td_code(prefix_hash.as_deref().unwrap_or("—")));
            out.push_str("</tr>");
        }
    }
    out.push_str("</tbody></table>");
    if n == 0 {
        out.push_str("<p class=warn>No ContextAssembly events found.</p>");
    }
    out
}

fn render_compaction_list(
    state: &AppState,
    daemon_id: &str,
    session_id: &str,
    events: &[SessionEvent],
) -> String {
    let mut out = String::from("<h2>CompactionAssembly</h2><table><thead><tr><th>call</th><th>event</th><th>policy</th><th>before</th><th>after</th><th>decisions</th><th>wall us</th></tr></thead><tbody>");
    let mut n = 0;
    for ev in events {
        if let EventPayload::CompactionAssembly {
            model_call_id,
            policy_id,
            tokens_before,
            tokens_after,
            decisions,
            wall_clock_us,
        } = &ev.payload
        {
            n += 1;
            let href = state.href(&format!(
                "/sessions/{}/{}/compactions/{}",
                urlish(daemon_id),
                urlish(session_id),
                model_call_id
            ));
            out.push_str("<tr>");
            out.push_str(&format!(
                "<td><a href=\"{}\">{}</a></td>",
                esc_attr(&href),
                model_call_id
            ));
            out.push_str(&td_num(ev.id));
            out.push_str(&td(policy_id));
            out.push_str(&td_num(*tokens_before));
            out.push_str(&td_num(*tokens_after));
            out.push_str(&td_num(decisions.len()));
            out.push_str(&td_num(*wall_clock_us));
            out.push_str("</tr>");
        }
    }
    out.push_str("</tbody></table>");
    if n == 0 {
        out.push_str("<p class=warn>No CompactionAssembly events found.</p>");
    }
    out
}

// ── mu-cc-sessions-console-lqqt.2: claude-code detail view ────────────
//
// The native detail view (`render_session_page`) reads mu's typed event
// log from `events_dir`. cc sessions live elsewhere (the cc projects
// dir) and have a different on-disk shape, so they get a parallel render
// path that maps cc content blocks onto the same `transcript_block`
// primitive. The header's mark line is a placeholder: cc marks land in a
// task_log sidecar (bead .3), wired at merge — never appended to the cc
// transcript file (READ-ONLY invariant).

/// Reject path-ish id segments before joining them into a filesystem
/// path under the cc projects dir — the cc reader walks into
/// `~/.claude-personal`, so a `..` segment must never escape it.
fn cc_id_ok(s: &str) -> bool {
    !s.is_empty() && !s.contains(['/', '\\']) && !s.contains("..")
}

pub(crate) fn render_cc_session_page(
    state: Arc<AppState>,
    project_dir: String,
    session_id: String,
    tab: CcDetailTab,
) -> Html<String> {
    if !cc_id_ok(&project_dir) || !cc_id_ok(&session_id) {
        return page(
            &state,
            "bad request",
            "<h1>bad request</h1><p class=err>invalid session path</p>",
        );
    }
    let Some(projects_dir) = state.cc_projects_dir.clone() else {
        return page(
            &state,
            "session not found",
            "<h1>session not found</h1><p class=err>claude-code scanning is not enabled on this console.</p>",
        );
    };
    let path = projects_dir
        .join(&project_dir)
        .join(format!("{session_id}.jsonl"));
    match read_cc_transcript(&path) {
        Ok(tx) => {
            let mut body = cc_session_header(&state, &project_dir, &session_id, &tx);
            body.push_str(&cc_session_nav(&state, &project_dir, &session_id));
            match tab {
                CcDetailTab::Transcript => body.push_str(&render_cc_transcript(&tx)),
                CcDetailTab::Events => body.push_str(&render_cc_events(&tx)),
                CcDetailTab::Cost => body.push_str(&render_cc_cost(&tx)),
            }
            page(&state, &session_id, &body)
        }
        Err(e) => page(
            &state,
            "session not found",
            &format!(
                "<h1>session not found</h1><p class=err>{}</p>",
                esc(&e.to_string())
            ),
        ),
    }
}

fn cc_session_header(
    state: &AppState,
    project_dir: &str,
    session_id: &str,
    tx: &CcTranscript,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "<p><a href=\"{}\">← sessions</a></p>",
        esc_attr(&state.href("/sessions"))
    ));
    out.push_str(&format!(
        "<h1><code>{}</code></h1><p class=muted>claude-code · project <code>{}</code> · {} entr(ies)</p>",
        esc(session_id),
        esc(project_dir),
        tx.entries.len()
    ));
    if let Some(model) = &tx.model {
        out.push_str(&format!(
            "<p class=muted>model <code>{}</code></p>",
            esc(model)
        ));
    }
    if tx.malformed_lines > 0 {
        out.push_str(&format!(
            "<p class=warn>{} malformed line(s) skipped while reading this transcript.</p>",
            tx.malformed_lines
        ));
    }
    // mu-y5hz: subagent (sidechain) turns stay rendered in the transcript
    // but are excluded from the cost-tab total; note their presence here.
    if tx.sidechain_entries > 0 {
        out.push_str(&format!(
            "<p class=muted>{} subagent (sidechain) turn(s) present — rendered below, excluded from cost totals.</p>",
            tx.sidechain_entries
        ));
    }
    // cc marks live in a task_log sidecar (bead .3), never in the
    // transcript file. Until that seam lands the line is a placeholder;
    // the director wires it at merge.
    out.push_str(
        "<p class=muted>mark: — <span class=muted>(claude-code marks land via the task_log sidecar — bead mu-cc-sessions-console-lqqt.3)</span></p>",
    );
    out
}

fn cc_session_nav(state: &AppState, project_dir: &str, session_id: &str) -> String {
    let root = format!("/cc/{}/{}", urlish(project_dir), urlish(session_id));
    let items = [
        ("overview", root.clone()),
        ("raw json", format!("{root}/events")),
        ("cost", format!("{root}/cost")),
    ];
    let mut out = String::from("<nav class=tabs>");
    for (label, path) in items {
        out.push_str(&format!(
            "<a href=\"{}\">{}</a>",
            esc_attr(&state.href(&path)),
            esc(label)
        ));
    }
    out.push_str("</nav>");
    out
}

fn render_cc_transcript(tx: &CcTranscript) -> String {
    let mut out = String::from(
        "<h2>transcript</h2>\
         <div class=toolbar>\
           <label><input type=checkbox checked onchange=\"toggleRole('user', this.checked)\"> user</label>\
           <label><input type=checkbox checked onchange=\"toggleRole('assistant', this.checked)\"> assistant</label>\
           <label><input type=checkbox checked onchange=\"toggleRole('tool', this.checked)\"> tools</label>\
           <button type=button onclick=\"expandAll('#transcript')\">expand all</button>\
           <button type=button onclick=\"collapseAll('#transcript')\">collapse all</button>\
           <label><input type=checkbox checked onchange=\"setTranscriptBodyScroll(this.checked)\"> scroll bodies</label>\
         </div><div id=transcript>",
    );
    for e in &tx.entries {
        // Meta envelopes (summary/attachment/unknown) have no rendered
        // text — show their raw JSON so the line degrades visibly rather
        // than vanishing. Open tool_result/meta blocks by default? No —
        // keep all collapsed, matching the native transcript.
        let text = if e.role == CcRole::Meta {
            format!("[{}]\n{}", e.envelope_type, e.raw)
        } else {
            e.text.clone()
        };
        transcript_block(
            &mut out,
            e.seq as u64,
            e.timestamp_unix_ms.unwrap_or(0),
            e.role.css(),
            &text,
            false,
        );
    }
    out.push_str("</div>");
    if tx.entries.is_empty() {
        out.push_str("<p class=warn>No transcript entries in this file.</p>");
    }
    out
}

fn render_cc_events(tx: &CcTranscript) -> String {
    let mut out = String::from("<h2>raw transcript lines</h2><div class=toolbar><button type=button onclick=\"expandAll('#events')\">expand json</button><button type=button onclick=\"collapseAll('#events')\">collapse json</button></div><table id=events><thead><tr><th>#</th><th>time</th><th>type</th><th>role</th><th>json</th></tr></thead><tbody>");
    for e in &tx.entries {
        out.push_str(&format!("<tr id=\"event-{}\">", e.seq));
        out.push_str(&format!("<td>{}</td>", event_anchor(e.seq as u64)));
        out.push_str(&td_time(e.timestamp_unix_ms));
        out.push_str(&td_code(&e.envelope_type));
        out.push_str(&td(e.role.css()));
        out.push_str(&format!(
            "<td>{}</td>",
            truncated_details("json", &e.raw, 40_000)
        ));
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table>");
    if tx.malformed_lines > 0 {
        out.push_str(&format!(
            "<p class=warn>{} malformed line(s) could not be parsed and are not shown.</p>",
            tx.malformed_lines
        ));
    }
    out
}

fn render_cc_cost(tx: &CcTranscript) -> String {
    let mut out = String::from("<h2>cost/cache</h2><p class=muted>Per-assistant-turn usage from <code>message.usage</code>. cache-read inflates across turns (re-read each call), same as the native cost view's sum.</p>");
    let mut total = Usage::default();
    let mut any = false;
    out.push_str("<table><thead><tr><th>#</th><th>model</th><th>input</th><th>output</th><th>cache read</th><th>cache write</th><th>5m write</th><th>1h write</th></tr></thead><tbody>");
    for e in &tx.entries {
        let Some(u) = e.usage else { continue };
        any = true;
        // mu-y5hz policy (a): a sidechain (subagent) turn's usage is
        // excluded from the summed total — same exclusion the index
        // scanner applies — but the row stays visible (marked) rather than
        // dropped, so the per-turn detail is still inspectable.
        if !e.is_sidechain {
            total = total + u;
        }
        out.push_str("<tr>");
        out.push_str(&td_num(e.seq));
        let model_cell = match e.model.as_deref() {
            Some(m) if e.is_sidechain => format!("{} (sidechain)", esc(m)),
            Some(m) => esc(m),
            None if e.is_sidechain => "— (sidechain)".to_string(),
            None => "—".to_string(),
        };
        out.push_str(&format!("<td>{model_cell}</td>"));
        out.push_str(&td_num(u.input_tokens));
        out.push_str(&td_num(u.output_tokens));
        out.push_str(&td(&fmt_opt_u64(u.cache_read_input_tokens)));
        out.push_str(&td(&fmt_opt_u64(u.cache_creation_input_tokens)));
        out.push_str(&td(&fmt_opt_u64(u.cache_creation_5m_input_tokens)));
        out.push_str(&td(&fmt_opt_u64(u.cache_creation_1h_input_tokens)));
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table>");
    if any {
        out.push_str("<h3>summed buckets</h3><dl class=kv>");
        kv(&mut out, "input", &total.input_tokens.to_string());
        kv(&mut out, "output", &total.output_tokens.to_string());
        kv(
            &mut out,
            "cache read",
            &fmt_opt_u64(total.cache_read_input_tokens),
        );
        kv(
            &mut out,
            "cache write",
            &fmt_opt_u64(total.cache_creation_input_tokens),
        );
        kv(
            &mut out,
            "5m write",
            &fmt_opt_u64(total.cache_creation_5m_input_tokens),
        );
        kv(
            &mut out,
            "1h write",
            &fmt_opt_u64(total.cache_creation_1h_input_tokens),
        );
        out.push_str("</dl>");
    } else {
        out.push_str("<p class=warn>No usage records found in this transcript.</p>");
    }
    // mu-y5hz: make the exclusion explicit so the summed total reading
    // lower than the per-row sum isn't a surprise.
    if tx.sidechain_entries > 0 {
        out.push_str(&format!(
            "<p class=muted>{} subagent (sidechain) turn(s) shown above are excluded from the summed total.</p>",
            tx.sidechain_entries
        ));
    }
    out
}
