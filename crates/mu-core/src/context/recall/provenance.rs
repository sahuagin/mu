//! Recall provenance — mu-recall-provenance-audit-vnc9.1 (P0).
//!
//! Builds the [`EventPayload::RecallProvenance`] event from the
//! session-start [`ProjectContext`]: one ref per injected span —
//! `{source, content-hash, token-count}` — never the text. Sensitive
//! spans (today: [`RecallSource::Memory`], the personal identity
//! kernel) become redacted-tombstones: ref + source-type only, no
//! name, no content. See `specs/rfc-recall-provenance-audit.md`.
//!
//! Token counts and content hashes are computed here at emission time,
//! NOT stored on [`RecalledItem`] — the estimate is a property of the
//! audit record, and providers stay untouched.

use crate::context::compaction::estimate_text_tokens;
use crate::event_log::{EventPayload, RecallProvenanceEntry, RecallSourceKind};

use super::{ProjectContext, RecallSource, RecalledItem};

/// Build the provenance ref for one injected span. Redaction keys off
/// [`RecallSource::sensitive`] (RFC principle 5): sensitive ⇒ no name
/// (granularity `hash + source-type`); non-sensitive ⇒ name/path in
/// the clear (`ProjectFile`'s canonical path; `Bootloader` has none).
pub fn provenance_entry(item: &RecalledItem) -> RecallProvenanceEntry {
    let redacted = item.source.sensitive();
    let (source, name) = match &item.source {
        RecallSource::Memory => (RecallSourceKind::Memory, None),
        RecallSource::ProjectFile { path } => (
            RecallSourceKind::ProjectFile,
            (!redacted).then(|| path.to_string_lossy().into_owned()),
        ),
        RecallSource::Bootloader => (RecallSourceKind::Bootloader, None),
    };
    RecallProvenanceEntry {
        source,
        stable_id: item.stable_id.to_string(),
        content_hash: blake3::hash(item.content.as_bytes()).to_hex().to_string(),
        token_count: estimate_text_tokens(&item.content) as u64,
        redacted,
        name,
    }
}

/// Build the [`EventPayload::RecallProvenance`] event for a session's
/// recall injection set. Callers should skip emission entirely when
/// recall produced nothing (`build_project_context` returns `None`) —
/// absence in the log means nothing was injected.
pub fn recall_provenance_payload(ctx: &ProjectContext) -> EventPayload {
    EventPayload::RecallProvenance {
        items: ctx.items.iter().map(provenance_entry).collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::event_log::{EventActor, SessionEvent};

    fn item(source: RecallSource, content: &str, stable_id: &str) -> RecalledItem {
        RecalledItem {
            source,
            content: Arc::from(content),
            stable_id: stable_id.into(),
        }
    }

    #[test]
    fn memory_entry_is_redacted_with_no_name() {
        let entry = provenance_entry(&item(
            RecallSource::Memory,
            "## Identity\npersonal facts here",
            "memory-3f9c2ab81d04",
        ));
        assert_eq!(entry.source, RecallSourceKind::Memory);
        assert!(entry.redacted);
        assert_eq!(entry.name, None);
        assert_eq!(entry.stable_id, "memory-3f9c2ab81d04");
        assert_eq!(
            entry.content_hash,
            blake3::hash("## Identity\npersonal facts here".as_bytes())
                .to_hex()
                .to_string()
        );
        assert!(entry.token_count > 0);
    }

    #[test]
    fn project_file_entry_is_plain_with_path_name() {
        let entry = provenance_entry(&item(
            RecallSource::ProjectFile {
                path: PathBuf::from("/p/MU.md"),
            },
            "# MU.md\nbuild with just ci",
            "file-77aa01bc23de",
        ));
        assert_eq!(entry.source, RecallSourceKind::ProjectFile);
        assert!(!entry.redacted);
        assert_eq!(entry.name.as_deref(), Some("/p/MU.md"));
    }

    #[test]
    fn bootloader_entry_is_plain_with_no_name() {
        let entry = provenance_entry(&item(
            RecallSource::Bootloader,
            "you are mu",
            "bootloader-5d2e91aa07bc",
        ));
        assert_eq!(entry.source, RecallSourceKind::Bootloader);
        assert!(!entry.redacted);
        assert_eq!(entry.name, None);
    }

    /// Tamper-evidence: ProjectFile's stable_id is a PATH hash, so the
    /// same path with different content keeps its stable_id while
    /// content_hash diverges — that divergence IS the detection.
    #[test]
    fn project_file_content_change_changes_content_hash_not_stable_id() {
        let path = RecallSource::ProjectFile {
            path: PathBuf::from("/p/MU.md"),
        };
        let a = provenance_entry(&item(path.clone(), "version one", "file-77aa01bc23de"));
        let b = provenance_entry(&item(path.clone(), "version two", "file-77aa01bc23de"));
        let a2 = provenance_entry(&item(path, "version one", "file-77aa01bc23de"));
        assert_eq!(a.stable_id, b.stable_id);
        assert_ne!(a.content_hash, b.content_hash);
        assert_eq!(a.content_hash, a2.content_hash);
    }

    /// The no-content-leak property (the P0 verification gate): the
    /// serialized event for a memory-sourced span must not contain the
    /// span's content.
    #[test]
    fn serialized_event_leaks_no_memory_content() {
        const SENTINEL: &str = "SENTINEL-PERSONAL-FACT-zq9";
        let content = format!("## Identity\n{SENTINEL}\nmore personal context");
        let payload = recall_provenance_payload(&ProjectContext {
            items: vec![item(RecallSource::Memory, &content, "memory-3f9c2ab81d04")],
        });
        let event = SessionEvent {
            id: 2,
            session_id: "sess-01".to_string(),
            parent_event_ids: Vec::new(),
            timestamp_unix_ms: 1_765_500_000_000,
            actor: EventActor::System,
            payload,
        };
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(!json.contains(SENTINEL), "content leaked into event JSON");
        assert!(
            !json.contains("## Identity"),
            "content fragment leaked into event JSON"
        );
        assert!(json.contains("\"redacted\":true"));
    }

    #[test]
    fn render_marker_pins_exact_strings() {
        let redacted = RecallProvenanceEntry {
            source: RecallSourceKind::Memory,
            stable_id: "memory-3f9c2ab81d04".to_string(),
            content_hash: "00".repeat(32),
            token_count: 987,
            redacted: true,
            name: None,
        };
        assert_eq!(
            redacted.render_marker(),
            "[redacted span ref=memory-3f9c2ab81d04 source=memory tokens=987]"
        );
        let plain = RecallProvenanceEntry {
            source: RecallSourceKind::ProjectFile,
            stable_id: "file-77aa01bc23de".to_string(),
            content_hash: "00".repeat(32),
            token_count: 412,
            redacted: false,
            name: Some("/p/MU.md".to_string()),
        };
        assert_eq!(
            plain.render_marker(),
            "[span ref=file-77aa01bc23de source=project_file name=/p/MU.md tokens=412]"
        );
    }

    #[test]
    fn sensitive_is_true_for_memory_only() {
        assert!(RecallSource::Memory.sensitive());
        assert!(!RecallSource::ProjectFile {
            path: PathBuf::from("/p/MU.md")
        }
        .sensitive());
        assert!(!RecallSource::Bootloader.sensitive());
    }
}
