//! Mailbox and peer request handlers (mailbox.*, peer.*).

use serde_json::Value;

use mu_core::event_log::{EventActor, EventPayload, SessionEventLog};
use mu_core::protocol::{
    MailboxConsumeRequest, MailboxConsumeResponse, MailboxListRequest, MailboxListResponse,
    MailboxMessageView, MailboxPostRequest, MailboxPostResponse, MailboxReadRequest,
    MailboxReadResponse, PeerHelloRequest, PeerHelloResponse, Request, Response,
};
use mu_core::transport::{codes, err_response, ok_response, NotificationWriter};

use crate::serve::daemon_info::DaemonInfo;
use crate::serve::sessions::Sessions;

use super::to_value_or_null;

/// `peer.hello` — A asks B for a peer handle. v1 policy: accept any
/// same-daemon peer whose `want.method` is `"mailbox.post"`. The
/// target session issues a fresh opaque token with
/// `allowed_methods = {mailbox.post}` and no expiry. Future iterations
/// make this policy programmable per-target-session.
pub fn handle_peer_hello(
    request: Request<Value>,
    sessions: Sessions,
    _daemon_info: DaemonInfo,
) -> Response<Value> {
    let params: PeerHelloRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("peer.hello: invalid params: {e}"),
            );
        }
    };

    // Target must exist.
    let mailbox = match sessions.mailbox(&params.to_session_id) {
        Some(m) => m,
        None => {
            return ok_response(
                request.id,
                to_value_or_null(PeerHelloResponse::Denied {
                    reason: format!("unknown target session: {}", params.to_session_id),
                }),
            );
        }
    };

    // v1 default policy: accept only `mailbox.post`.
    let response = if params.want.method == MailboxPostRequest::METHOD {
        let allowed: std::collections::HashSet<String> =
            std::iter::once(MailboxPostRequest::METHOD.to_owned()).collect();
        let token = mailbox.issue_handle(
            params.from.session_id.clone(),
            allowed.clone(),
            None, // no expiry in Phase 1
            None, // no per-handle call budget in Phase 1
        );
        PeerHelloResponse::Accepted {
            peer_handle: token,
            allowed_methods: allowed.into_iter().collect(),
            expires_at_unix_ms: None,
        }
    } else {
        PeerHelloResponse::Denied {
            reason: format!(
                "v1 policy refuses method `{}`; only `mailbox.post` is offered",
                params.want.method,
            ),
        }
    };

    ok_response(request.id, to_value_or_null(response))
}

/// `mailbox.post` — peer A drops a message into B's mailbox. Requires
/// a valid peer handle previously obtained from `peer.hello`. Appends
/// a `MailboxMessagePosted` event to the target session's event log
/// and emits a `session.mailbox_message` wire notification.
pub async fn handle_mailbox_post(
    request: Request<Value>,
    sessions: Sessions,
    notif: NotificationWriter,
    daemon_info: DaemonInfo,
) -> Response<Value> {
    let params: MailboxPostRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("mailbox.post: invalid params: {e}"),
            );
        }
    };

    let target_mailbox = match sessions.mailbox(&params.to_session_id) {
        Some(m) => m,
        None => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("session not found: {}", params.to_session_id),
            );
        }
    };

    // Authorization: require a valid peer handle issued by target.
    // Note: same-daemon trust intentionally NOT applied here — even
    // when sender and recipient are in-process, the handshake must
    // happen first. This avoids carving a Phase-1-only shortcut.
    if target_mailbox
        .check_handle(
            &params.peer_handle,
            &params.from.session_id,
            MailboxPostRequest::METHOD,
        )
        .is_none()
    {
        return err_response(
            request.id,
            codes::INVALID_PARAMS,
            "mailbox.post: invalid or expired peer handle".to_string(),
        );
    }

    // Verify the claimed `from.daemon_id` matches this daemon. Phase
    // 1 is single-daemon; future Phase 2 cross-daemon will gate this
    // differently.
    if params.from.daemon_id != daemon_info.daemon_id() {
        return err_response(
            request.id,
            codes::INVALID_PARAMS,
            format!(
                "mailbox.post: from.daemon_id `{}` does not match this daemon",
                params.from.daemon_id
            ),
        );
    }

    let log = match sessions.event_log(&params.to_session_id) {
        Some(l) => l,
        None => {
            // Race: session vanished between `mailbox()` and now.
            // Treat as "session not found."
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                "mailbox.post: target session no longer exists".to_string(),
            );
        }
    };

    let seq = target_mailbox.allocate_seq();
    // EventActor for a peer-originated post: the daemon mediated the
    // append, so `System` is the closest available variant. Peer
    // identity is carried in the payload's `from_daemon_id` /
    // `from_session_id` fields. A future spec can add
    // `EventActor::Peer { daemon_id, session_id }` if needed.
    let posted_event_id = log.append(
        EventActor::System,
        EventPayload::MailboxMessagePosted {
            seq,
            from_daemon_id: params.from.daemon_id.clone(),
            from_session_id: params.from.session_id.clone(),
            message_kind: params.kind.clone(),
            subject: params.subject.clone(),
            body: params.body.clone(),
            expires_at_unix_ms: params.expires_at_unix_ms,
        },
    );
    let posted_at_unix_ms = log
        .snapshot()
        .iter()
        .find(|e| e.id == posted_event_id)
        .map(|e| e.timestamp_unix_ms)
        .unwrap_or(0);

    // Wire notification — Phase 4 TUI (F9 mailbox view) subscribes.
    let notif_payload = mu_core::protocol::MailboxMessageEvent {
        session_id: params.to_session_id.clone(),
        seq,
        from_daemon_id: params.from.daemon_id.clone(),
        from_session_id: params.from.session_id.clone(),
        kind: params.kind.clone(),
        subject: params.subject.clone(),
        body: params.body.clone(),
        posted_at_unix_ms,
        expires_at_unix_ms: params.expires_at_unix_ms,
    };
    if let Ok(value) = serde_json::to_value(&notif_payload) {
        let _ = notif
            .emit(mu_core::protocol::MailboxMessageEvent::METHOD, value)
            .await;
    }

    // mu-slat Phase 2: if the target is a live session with an agent
    // loop, inject a MailboxMessage input so the LLM wakes up and
    // processes the message. No polling needed — the post IS the trigger.
    if let Some(tx) = sessions.input_sender(&params.to_session_id) {
        let _ = tx.try_send(mu_core::agent::AgentInput::MailboxMessage {
            from_session_id: params.from.session_id.clone(),
            message_kind: params.kind.clone(),
            subject: params.subject.clone(),
            seq,
        });
    }

    // mu-slat Phase 3: if the SENDER is a pot worker reporting its
    // result, reap it host-side. Interactive claude doesn't exit on its
    // own (it idles at the prompt pegging cores until the deadline), and
    // its in-pot hook can't self-kill under linuxulator. Killing the pty
    // here makes claude exit → EOF → monitor_worker's normal exit path.
    if params.kind == "result" {
        let _ = sessions.reap_worker(&params.from.session_id);
    }

    ok_response(
        request.id,
        to_value_or_null(MailboxPostResponse { posted: true, seq }),
    )
}

/// `mailbox.list` — read a session's mailbox. Projects from the event
/// log: posts minus consumed. Self-access (a session listing its own
/// mailbox) doesn't require a handle; cross-session listing does.
pub fn handle_mailbox_list(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    let params: MailboxListRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("mailbox.list: invalid params: {e}"),
            );
        }
    };

    let log = match sessions.event_log(&params.session_id) {
        Some(l) => l,
        None => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("session not found: {}", params.session_id),
            );
        }
    };

    let messages = project_mailbox(&log, params.since_seq, params.include_consumed);
    ok_response(
        request.id,
        to_value_or_null(MailboxListResponse { messages }),
    )
}

/// `mailbox.read` — fetch a single message's full view by seq.
/// Self-access doesn't require a handle; cross-session read does.
pub fn handle_mailbox_read(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    let params: MailboxReadRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("mailbox.read: invalid params: {e}"),
            );
        }
    };

    let log = match sessions.event_log(&params.session_id) {
        Some(l) => l,
        None => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("session not found: {}", params.session_id),
            );
        }
    };

    let message = project_mailbox(&log, None, true)
        .into_iter()
        .find(|m| m.seq == params.seq);

    ok_response(
        request.id,
        to_value_or_null(MailboxReadResponse { message }),
    )
}

/// `mailbox.consume` — mark messages as consumed. Each unknown or
/// already-consumed seq is silently skipped; the response reports
/// how many transitioned.
pub fn handle_mailbox_consume(request: Request<Value>, sessions: Sessions) -> Response<Value> {
    let params: MailboxConsumeRequest = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(e) => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("mailbox.consume: invalid params: {e}"),
            );
        }
    };

    let log = match sessions.event_log(&params.session_id) {
        Some(l) => l,
        None => {
            return err_response(
                request.id,
                codes::INVALID_PARAMS,
                format!("session not found: {}", params.session_id),
            );
        }
    };

    // Compute current consumed-set and posted-set from the log to
    // skip duplicates / unknowns.
    let (posted_seqs, consumed_seqs) = posted_and_consumed_sets(&log);
    let mut consumed_count: u32 = 0;
    for seq in &params.seqs {
        if !posted_seqs.contains(seq) {
            continue; // unknown — skip
        }
        if consumed_seqs.contains(seq) {
            continue; // already consumed — skip
        }
        log.append(
            EventActor::System,
            EventPayload::MailboxMessageConsumed { seq: *seq },
        );
        consumed_count = consumed_count.saturating_add(1);
    }

    ok_response(
        request.id,
        to_value_or_null(MailboxConsumeResponse { consumed_count }),
    )
}

/// Project the mailbox view from a session's event log. Pure function;
/// no IO. Walks the log once gathering posted entries and a consumed
/// set, then composes the final `MailboxMessageView` list filtering
/// per `since_seq` and `include_consumed`. Order is by `seq` ascending
/// (which equals event-log append order since `seq` is monotonic).
fn project_mailbox(
    log: &SessionEventLog,
    since_seq: Option<u64>,
    include_consumed: bool,
) -> Vec<MailboxMessageView> {
    let events = log.snapshot();
    let mut consumed = std::collections::HashSet::<u64>::new();
    for ev in events.iter().rev() {
        if let EventPayload::MailboxMessageConsumed { seq } = &ev.payload {
            consumed.insert(*seq);
        }
    }
    let mut out: Vec<MailboxMessageView> = Vec::new();
    for ev in &events {
        if let EventPayload::MailboxMessagePosted {
            seq,
            from_daemon_id,
            from_session_id,
            message_kind,
            subject,
            body,
            expires_at_unix_ms,
        } = &ev.payload
        {
            if let Some(threshold) = since_seq {
                if *seq < threshold {
                    continue;
                }
            }
            let was_consumed = consumed.contains(seq);
            if was_consumed && !include_consumed {
                continue;
            }
            out.push(MailboxMessageView {
                seq: *seq,
                from_daemon_id: from_daemon_id.clone(),
                from_session_id: from_session_id.clone(),
                kind: message_kind.clone(),
                subject: subject.clone(),
                body: body.clone(),
                posted_at_unix_ms: ev.timestamp_unix_ms,
                consumed: was_consumed,
                expires_at_unix_ms: *expires_at_unix_ms,
            });
        }
    }
    out
}

/// Helper: gather the (posted_seqs, consumed_seqs) sets in one pass.
fn posted_and_consumed_sets(
    log: &SessionEventLog,
) -> (
    std::collections::HashSet<u64>,
    std::collections::HashSet<u64>,
) {
    let mut posted = std::collections::HashSet::<u64>::new();
    let mut consumed = std::collections::HashSet::<u64>::new();
    for ev in log.snapshot().iter() {
        match &ev.payload {
            EventPayload::MailboxMessagePosted { seq, .. } => {
                posted.insert(*seq);
            }
            EventPayload::MailboxMessageConsumed { seq } => {
                consumed.insert(*seq);
            }
            _ => {}
        }
    }
    (posted, consumed)
}
