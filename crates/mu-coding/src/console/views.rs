use std::sync::Arc;

use axum::response::Html;
use mu_core::{
    agent::{ContentBlock, Usage},
    event_log::{EventPayload, SessionEvent},
};

use crate::console::{
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

pub(crate) fn render_sessions_index(state: Arc<AppState>) -> Html<String> {
    let scan = scan_all(&state.events_dir, state.cc_projects_dir.as_deref());
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
    body.push_str("<table><thead><tr><th>last</th><th>daemon</th><th>session</th><th>mark</th><th>provider</th><th>model</th><th>asks</th><th>calls</th><th>tools</th><th>input</th><th>output</th><th>cache read</th><th>cache write</th></tr></thead><tbody>");
    for s in scan.sessions {
        let href = state.href(&format!(
            "/sessions/{}/{}",
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
        body.push_str(&td(&s.model.unwrap_or_else(|| "—".into())));
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
