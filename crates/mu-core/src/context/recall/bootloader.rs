//! Bootloader recall provider (bead mu-recall-bootloader-flag-nxpo).
//!
//! Emits a single fixed startup-orientation preamble as the FIRST recall
//! segment, before [`super::SubprocessRecallProvider`] (the identity
//! kernel) and [`super::ProjectFileRecallProvider`] (the CLAUDE.md /
//! AGENTS.md hierarchy). Position is the point: the orientation-about-the-
//! orientation must precede the orientation, so the serve loop pushes this
//! provider to the front of the recall chain when
//! [`crate::config::Config::bootloader_enabled`] (and recall is enabled).
//!
//! Unlike the other v0 providers this one touches neither the filesystem
//! nor a subprocess — its content is the operator-approved text resolved
//! at construction time ([`crate::config::Config::bootloader_text`]), so
//! `recall` is pure and deterministic.

use std::path::Path;

use crate::capability::Capability;
use crate::context::recall::{RecallProvider, RecallSource, RecalledItem};
use crate::context::rope::SpanText;

/// Recall provider that emits the fixed first-position bootloader preamble.
///
/// Constructed with the already-resolved text (config override or
/// [`crate::config::DEFAULT_BOOTLOADER_TEXT`]); the resolution lives in
/// [`crate::config::Config::bootloader_text`] so this type stays a dumb
/// emitter.
#[derive(Debug, Clone)]
pub struct BootloaderRecallProvider {
    text: SpanText,
}

impl BootloaderRecallProvider {
    /// Build a provider that emits `text` verbatim as its one recall item.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into().into(),
        }
    }
}

impl RecallProvider for BootloaderRecallProvider {
    fn recall(&self, _cwd: &Path, _capability: &Capability) -> Vec<RecalledItem> {
        // Stable id: short blake3 hash over the preamble so the rope span
        // id is deterministic for identical text (rope dedup / audit),
        // mirroring SubprocessRecallProvider's `memory-<hash12>` scheme.
        let hash = blake3::hash(self.text.as_bytes());
        let stable_id: SpanText = format!("bootloader-{}", &hash.to_hex().as_str()[..12]).into();
        vec![RecalledItem {
            source: RecallSource::Bootloader,
            content: self.text.clone(),
            stable_id,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_BOOTLOADER_TEXT;

    #[test]
    fn emits_single_bootloader_item_with_verbatim_text() {
        let provider = BootloaderRecallProvider::new(DEFAULT_BOOTLOADER_TEXT);
        let items = provider.recall(Path::new("/tmp"), &Capability::root());
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert!(matches!(item.source, RecallSource::Bootloader));
        assert_eq!(&*item.content, DEFAULT_BOOTLOADER_TEXT);
        assert!(item.stable_id.starts_with("bootloader-"));
        assert_eq!(item.stable_id.len(), "bootloader-".len() + 12);
    }

    #[test]
    fn recall_is_deterministic_and_cwd_independent() {
        let provider = BootloaderRecallProvider::new("orientation");
        let a = provider.recall(Path::new("/tmp"), &Capability::root());
        let b = provider.recall(Path::new("/elsewhere"), &Capability::root());
        assert_eq!(a[0].stable_id, b[0].stable_id);
        assert_eq!(a[0].content, b[0].content);
    }
}
