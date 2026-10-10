//! Session channel topics: a session channel's TOPIC is its session's label.
//!
//! A session publishes a label on its etcd presence key (mu-166mh), and the
//! discovery sweep carries it here. The topic is where an IRC client shows it:
//! weechat's buffer title, the first thing on joining. The text is
//! `<label> — <peer id>`, so the full id the channel name abbreviates is there
//! too.
//!
//! Pure state, like the channel reconciler: the session reports what it sees
//! (a join, the topic the server reports) and sends the lines this decides.
//! Three rules shape it:
//!
//! - **Only on a change.** A sweep that finds nothing new sends nothing. Per
//!   channel this remembers the topic it last APPLIED and the topic the server
//!   last REPORTED, and sends only when the wanted text is neither.
//! - **A hand-set topic survives until the label changes.** Someone setting a
//!   topic moves only the reported one, so the next sweep sees the wanted text
//!   already applied and leaves it alone.
//! - **No label, no topic.** A channel whose session never published a label
//!   is left as it is. When a label is removed, the topic falls back to the bare
//!   peer id, but only on a channel whose topic this put there: the current one
//!   was applied this connection, or reads as gateway-written.
//!
//! A channel two peers fold onto is skipped: it has no single session to name.

use std::collections::HashMap;

use mu_peer::PeerId;

use crate::framing::LINE_BUDGET;
use crate::mapping::{channel_for, fold_nick, CaseMapping};

/// The topic for `peer`, within `max_bytes`: its label and its id, or the bare
/// id without one. A label that does not fit is cut, on a character boundary,
/// before the id is: the id is what says whose channel this is.
pub fn topic_text(peer: &PeerId, label: Option<&str>, max_bytes: usize) -> String {
    let id = printable_id(peer);
    let suffix = format!(" — {id}");
    let room = max_bytes.saturating_sub(suffix.len());
    match label.map(|l| cut(l, room).trim_end()) {
        Some(label) if !label.is_empty() => format!("{label}{suffix}"),
        _ => cut(&id, max_bytes).to_string(),
    }
}

/// `peer`'s id as it may appear in a topic: control characters dropped. Ids
/// come from `$SRV` metadata unvalidated, and a CR or LF here would end the
/// TOPIC line early and start another command as the gateway. The label is
/// already clean (`presence::clean_label`); this is the other half.
fn printable_id(peer: &PeerId) -> String {
    peer.to_string()
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

/// The longest prefix of `s` within `max_bytes`, ending on a char boundary.
fn cut(s: &str, max_bytes: usize) -> &str {
    let mut end = s.len().min(max_bytes);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Room for the topic in `TOPIC <channel> :<topic>\r\n` within one IRC line.
fn topic_budget(channel: &str) -> usize {
    LINE_BUDGET.saturating_sub("TOPIC  :\r\n".len() + channel.len())
}

/// Whether `topic` reads as one [`topic_text`] would write for `peer`.
fn reads_as_ours(topic: &str, peer: &PeerId) -> bool {
    let id = printable_id(peer);
    topic == id || topic.ends_with(&format!(" — {id}"))
}

#[derive(Debug, Default)]
struct ChannelTopic {
    /// What the server last said the topic is (RPL_TOPIC on join, or a TOPIC
    /// by anyone). None for no topic, or not heard yet.
    reported: Option<String>,
    /// What this connection last sent, or found already in place.
    applied: Option<String>,
    /// A TOPIC was sent and the server has not reported the channel's topic
    /// since. A refusal arriving now is the refusal of that TOPIC.
    pending: bool,
}

/// The topic state of every session channel the gateway is in.
pub struct Topics {
    prefix: String,
    cm: CaseMapping,
    channellen: usize,
    /// Folded channel → what is known about its topic.
    channels: HashMap<String, ChannelTopic>,
}

impl Topics {
    /// Topics for session channels named under `prefix`, folded under `cm`
    /// within `channellen` — the same derivation the reconciler joins by.
    pub fn new(prefix: &str, cm: CaseMapping, channellen: usize) -> Self {
        Topics {
            prefix: prefix.to_string(),
            cm,
            channellen,
            channels: HashMap::new(),
        }
    }

    /// The gateway (re)joined `channel`: whatever it knew of the topic is from
    /// before, and the RPL_TOPIC that follows the JOIN is the truth.
    pub fn joined(&mut self, channel: &str) {
        self.channels.remove(&fold_nick(channel, self.cm));
    }

    /// The server reported `channel`'s topic: RPL_TOPIC, RPL_NOTOPIC, or a
    /// TOPIC change by anyone, the gateway's own echo included. An empty topic
    /// is no topic.
    pub fn reported(&mut self, channel: &str, topic: Option<&str>) {
        let entry = self
            .channels
            .entry(fold_nick(channel, self.cm))
            .or_default();
        entry.reported = topic.filter(|t| !t.is_empty()).map(str::to_string);
        entry.pending = false;
    }

    /// The TOPIC for `channel` never reached the socket. Forget it was
    /// applied, so the next sweep sends it again.
    pub fn unsent(&mut self, channel: &str) {
        if let Some(state) = self.channels.get_mut(&fold_nick(channel, self.cm)) {
            state.applied = None;
            state.pending = false;
        }
    }

    /// The server refused an operator command in `channel`. True when a TOPIC
    /// sent there is the thing it refused, so the caller can say so. The text
    /// stays recorded as applied: retrying a refusal every sweep would only
    /// repeat it, and a label change or a rejoin tries again.
    pub fn refused(&mut self, channel: &str) -> bool {
        self.channels
            .get_mut(&fold_nick(channel, self.cm))
            .is_some_and(|state| std::mem::take(&mut state.pending))
    }

    /// The TOPIC lines to send now, as `(channel, text)`, sorted by channel.
    ///
    /// `label` answers a peer's label as `Some(label)`, `Some(None)` for a peer
    /// known to have none, or None when that is not known, which leaves its
    /// channel alone; `is_joined` whether the gateway is in a
    /// channel (a channel still pending its JOIN gets nothing yet, and its state
    /// is dropped, so a rejoin starts clean). The decision is recorded as
    /// applied when it is made, so a refused TOPIC (the gateway is not an
    /// operator there) is not retried every sweep; the next label change tries
    /// again. [`Topics::refused`] is how a refusal gets said.
    pub fn sync<'l>(
        &mut self,
        peers: &[PeerId],
        label: impl Fn(&PeerId) -> Option<Option<&'l str>>,
        is_joined: impl Fn(&str) -> bool,
    ) -> Vec<(String, String)> {
        // Folded channel → (display name, the peers deriving it).
        let mut owners: HashMap<String, (String, Vec<&PeerId>)> = HashMap::new();
        for peer in peers {
            if let Some(channel) = channel_for(peer, &self.prefix, self.channellen) {
                owners
                    .entry(fold_nick(&channel, self.cm))
                    .or_insert_with(|| (channel, Vec::new()))
                    .1
                    .push(peer);
            }
        }

        self.channels.retain(|folded, _| is_joined(folded));

        let mut out = Vec::new();
        for (folded, (display, owned_by)) in owners {
            let [peer] = owned_by.as_slice() else {
                continue; // a folded collision names no single session
            };
            if !is_joined(&display) {
                continue;
            }
            let Some(label) = label(peer) else {
                continue; // unknown: no lease right now, or no label read yet
            };
            let state = self.channels.entry(folded).or_default();
            if label.is_none()
                && state.applied.is_none()
                && !state
                    .reported
                    .as_deref()
                    .is_some_and(|t| reads_as_ours(t, peer))
            {
                continue; // never labelled, and the topic is not ours to reset
            }
            let want = topic_text(peer, label, topic_budget(&display));
            if state.applied.as_deref() == Some(want.as_str()) {
                continue;
            }
            state.applied = Some(want.clone());
            if state.reported.as_deref() == Some(want.as_str()) {
                continue; // already showing it
            }
            state.pending = true;
            out.push((display, want));
        }
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const PREFIX: &str = "#mu-";

    fn topics() -> Topics {
        Topics::new(PREFIX, CaseMapping::Rfc1459, 64)
    }

    fn chan(peer: &PeerId) -> String {
        channel_for(peer, PREFIX, 64).unwrap()
    }

    /// Run one sync with `labels`, every peer's channel joined.
    fn sync(t: &mut Topics, peers: &[PeerId], labels: &[(&PeerId, &str)]) -> Vec<(String, String)> {
        let labels: HashMap<PeerId, String> = labels
            .iter()
            .map(|(p, l)| ((*p).clone(), l.to_string()))
            .collect();
        let joined: HashSet<String> = peers
            .iter()
            .map(|p| fold_nick(&chan(p), CaseMapping::Rfc1459))
            .collect();
        t.sync(
            peers,
            |p| Some(labels.get(p).map(String::as_str)),
            |c| joined.contains(&fold_nick(c, CaseMapping::Rfc1459)),
        )
    }

    #[test]
    fn a_label_sets_the_topic_once() {
        let a = PeerId::parse("cc:aaaa1111");
        let mut t = topics();
        let peers = vec![a.clone()];
        assert_eq!(
            sync(&mut t, &peers, &[(&a, "irc topics")]),
            vec![(chan(&a), "irc topics — cc:aaaa1111".to_string())]
        );
        assert!(
            sync(&mut t, &peers, &[(&a, "irc topics")]).is_empty(),
            "no change, no line"
        );
    }

    #[test]
    fn a_changed_label_changes_the_topic() {
        let a = PeerId::parse("cc:aaaa1111");
        let mut t = topics();
        let peers = vec![a.clone()];
        sync(&mut t, &peers, &[(&a, "one")]);
        assert_eq!(
            sync(&mut t, &peers, &[(&a, "two")]),
            vec![(chan(&a), "two — cc:aaaa1111".to_string())]
        );
    }

    #[test]
    fn an_unlabelled_channel_is_left_alone() {
        let a = PeerId::parse("cc:aaaa1111");
        let mut t = topics();
        t.reported(&chan(&a), Some("set by hand"));
        assert!(sync(&mut t, std::slice::from_ref(&a), &[]).is_empty());
    }

    #[test]
    fn a_removed_label_falls_back_to_the_peer_id() {
        let a = PeerId::parse("cc:aaaa1111");
        let mut t = topics();
        let peers = vec![a.clone()];
        sync(&mut t, &peers, &[(&a, "one")]);
        assert_eq!(
            sync(&mut t, &peers, &[]),
            vec![(chan(&a), "cc:aaaa1111".to_string())]
        );
        assert!(sync(&mut t, &peers, &[]).is_empty());
    }

    /// After a gateway restart nothing was applied this connection, but a
    /// stale label topic the gateway wrote is still recognisable as its own.
    #[test]
    fn a_stale_gateway_topic_is_reset_after_a_restart() {
        let a = PeerId::parse("cc:aaaa1111");
        let mut t = topics();
        t.joined(&chan(&a));
        t.reported(&chan(&a), Some("old work — cc:aaaa1111"));
        assert_eq!(
            sync(&mut t, std::slice::from_ref(&a), &[]),
            vec![(chan(&a), "cc:aaaa1111".to_string())]
        );
    }

    #[test]
    fn a_topic_already_showing_is_not_resent() {
        let a = PeerId::parse("cc:aaaa1111");
        let mut t = topics();
        t.joined(&chan(&a));
        t.reported(&chan(&a), Some("irc — cc:aaaa1111"));
        assert!(sync(&mut t, std::slice::from_ref(&a), &[(&a, "irc")]).is_empty());
    }

    #[test]
    fn a_hand_set_topic_survives_until_the_label_changes() {
        let a = PeerId::parse("cc:aaaa1111");
        let mut t = topics();
        let peers = vec![a.clone()];
        sync(&mut t, &peers, &[(&a, "one")]);
        t.reported(&chan(&a), Some("operator's note"));
        assert!(sync(&mut t, &peers, &[(&a, "one")]).is_empty());
        assert_eq!(sync(&mut t, &peers, &[(&a, "two")]).len(), 1);
    }

    #[test]
    fn a_rejoin_sets_the_topic_again() {
        let a = PeerId::parse("cc:aaaa1111");
        let mut t = topics();
        let peers = vec![a.clone()];
        sync(&mut t, &peers, &[(&a, "one")]);
        t.joined(&chan(&a)); // the channel may have died while the gateway was out
        assert_eq!(sync(&mut t, &peers, &[(&a, "one")]).len(), 1);
    }

    #[test]
    fn a_refusal_is_attributed_only_to_a_topic_in_flight() {
        let a = PeerId::parse("cc:aaaa1111");
        let mut t = topics();
        let peers = vec![a.clone()];
        assert!(!t.refused(&chan(&a)), "nothing sent yet");
        sync(&mut t, &peers, &[(&a, "one")]);
        assert!(t.refused(&chan(&a)), "the TOPIC just sent was refused");
        assert!(!t.refused(&chan(&a)), "said once");
        assert!(
            sync(&mut t, &peers, &[(&a, "one")]).is_empty(),
            "not retried every sweep"
        );
        sync(&mut t, &peers, &[(&a, "two")]);
        t.reported(&chan(&a), Some("two — cc:aaaa1111"));
        assert!(
            !t.refused(&chan(&a)),
            "accepted, so a later refusal is not this one"
        );
    }

    /// A peer whose lease lapsed for a moment is unknown, not unlabelled: its
    /// topic stays.
    #[test]
    fn an_unknown_label_changes_nothing() {
        let a = PeerId::parse("cc:aaaa1111");
        let mut t = topics();
        let peers = vec![a.clone()];
        sync(&mut t, &peers, &[(&a, "one")]);
        let out = t.sync(&peers, |_| None, |_| true);
        assert!(out.is_empty());
    }

    #[test]
    fn a_long_label_is_cut_and_the_line_fits() {
        let a = PeerId::parse("cc:aaaa1111");
        let mut t = topics();
        let long = "é".repeat(400);
        let out = sync(&mut t, std::slice::from_ref(&a), &[(&a, long.as_str())]);
        let (channel, topic) = &out[0];
        let line = format!("TOPIC {channel} :{topic}\r\n");
        assert!(line.len() <= LINE_BUDGET, "{} bytes", line.len());
        assert!(
            topic.ends_with(" — cc:aaaa1111"),
            "the id survives: {topic}"
        );
    }

    /// A peer id carrying CR/LF cannot end the TOPIC line and start another
    /// command.
    #[test]
    fn a_peer_id_cannot_inject_a_command() {
        let evil = PeerId::parse("cc:a\r\nPRIVMSG #mu :owned");
        for label in [None, Some("work")] {
            let topic = topic_text(&evil, label, 400);
            assert!(!topic.contains('\r') && !topic.contains('\n'), "{topic:?}");
        }
    }

    #[test]
    fn a_channel_not_yet_joined_gets_nothing() {
        let a = PeerId::parse("cc:aaaa1111");
        let mut t = topics();
        let labels = HashMap::from([(a.clone(), "one".to_string())]);
        let peers = vec![a.clone()];
        let out = t.sync(
            &peers,
            |p| Some(labels.get(p).map(String::as_str)),
            |_| false,
        );
        assert!(out.is_empty());
    }
}
