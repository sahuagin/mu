//! Pure mesh↔IRC name mapping: nick folding, human identity, role aliases,
//! deterministic channel names, puppet nicks, and reverse resolution.
//!
//! Everything here is a pure function of its arguments. Reverse resolution of
//! CHANNELS takes the *current* discovered-peer snapshot as a parameter — it
//! never consults a stored roster — so a changing mesh yields changing results
//! with no hidden state (the gateway's "process state is disposable" rule).
//! Reverse resolution of puppet NICKS is the one deliberate exception: a nick
//! is granted by the server at registration, so which peer holds which nick is
//! a fact the pool learns, not one it can recompute — [`NickTable`] holds it
//! (specs/plans/mu-irc-gateway-v1-puppets.md, "Nick mapping contract").

use std::collections::HashMap;

use mu_peer::PeerId;

/// How an IRC server folds nick/channel case, from its advertised
/// `CASEMAPPING`. The default when a server advertises nothing is
/// [`CaseMapping::Rfc1459`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaseMapping {
    /// `A-Z` fold to `a-z` only.
    Ascii,
    /// `ascii`, plus `[` `]` `\` `~` fold to `{` `}` `|` `^` — the RFC 1459 rule
    /// where those are "uppercase" forms.
    #[default]
    Rfc1459,
    /// `rfc1459` without the `~`→`^` mapping (`~` and `^` stay distinct).
    StrictRfc1459,
}

/// Fold one character to lower case per `cm`.
fn fold_char(c: char, cm: CaseMapping) -> char {
    match c {
        'A'..='Z' => c.to_ascii_lowercase(),
        '[' if cm != CaseMapping::Ascii => '{',
        ']' if cm != CaseMapping::Ascii => '}',
        '\\' if cm != CaseMapping::Ascii => '|',
        '~' if cm == CaseMapping::Rfc1459 => '^',
        _ => c,
    }
}

/// Fold a name to its canonical form under `cm`. CASEMAPPING governs nicks and
/// channel names alike, so both go through this.
fn fold_name(name: &str, cm: CaseMapping) -> String {
    name.chars().map(|c| fold_char(c, cm)).collect()
}

/// Fold a nick to its canonical form under `cm`. Two nicks are the same IRC
/// identity iff their folded forms are byte-equal.
pub fn fold_nick(nick: &str, cm: CaseMapping) -> String {
    fold_name(nick, cm)
}

/// The gateway's OWN nick, kept in the spelling the server knows it by, with the
/// folded form derived on demand.
///
/// Folding is lossy and mapping-specific: under `rfc1459` `[` folds to `{`,
/// under `ascii` it does not. So a folded nick cannot be re-folded under a new
/// `CASEMAPPING` and still mean the same person — `gw[` folded to `gw{` under
/// rfc1459 stays `gw{` when the server switches to `ascii`, while the real nick
/// now folds to `gw[`. The gateway would then fail to recognize its own NAMES
/// entry, front ITSELF as a human, and stop treating its own PART as a
/// self-departure.
///
/// Holding the original spelling makes that unrepresentable: every fold starts
/// from the wire nick, so [`SelfNick::set_casemapping`] re-derives rather than
/// re-folds. Both the membership view and the outbound router use this one type
/// so the rule cannot drift between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfNick {
    original: String,
    folded: String,
}

impl SelfNick {
    /// The gateway's nick as the server spells it, folded under `cm`.
    pub fn new(original: &str, cm: CaseMapping) -> Self {
        SelfNick {
            original: original.to_string(),
            folded: fold_nick(original, cm),
        }
    }

    /// The wire spelling — what the server calls the gateway.
    pub fn original(&self) -> &str {
        &self.original
    }

    /// The folded form under the casemapping last applied. Compare observed
    /// nicks against this.
    pub fn folded(&self) -> &str {
        &self.folded
    }

    /// Whether `nick`, folded under `cm`, is the gateway itself.
    pub fn matches(&self, nick: &str, cm: CaseMapping) -> bool {
        fold_nick(nick, cm) == self.folded
    }

    /// The server changed `CASEMAPPING`: re-derive the folded form FROM THE
    /// ORIGINAL, never from the previous folded value.
    pub fn set_casemapping(&mut self, cm: CaseMapping) {
        self.folded = fold_nick(&self.original, cm);
    }

    /// The gateway renamed itself to `original` (a NICK the server accepted).
    pub fn rename(&mut self, original: &str, cm: CaseMapping) {
        self.original = original.to_string();
        self.folded = fold_nick(original, cm);
    }
}

/// A human operator's identity plus the account label the server reported for
/// them, kept SEPARATE from identity on purpose: identity is the folded nick
/// (what addresses the person on the mesh); the account is metadata that never
/// alters which `human:<nick>` peer this is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanIdentity {
    /// `human:<folded-nick>` — the mesh peer id.
    pub peer: PeerId,
    /// The verbatim `account` the server reported (via `account-tag`/WHOIS), if
    /// any. Not folded, not part of `peer`.
    pub account: Option<String>,
}

/// Build a `human:<nick>` peer id, folding the nick per `cm` first. Identity is
/// the folded nick only.
pub fn human_peer(nick: &str, cm: CaseMapping) -> PeerId {
    PeerId::human(fold_nick(nick, cm))
}

/// Build a [`HumanIdentity`] from a nick and an optional account label. The nick
/// is folded into the identity; the account is stored beside it, untouched.
pub fn human_identity(nick: &str, account: Option<&str>, cm: CaseMapping) -> HumanIdentity {
    HumanIdentity {
        peer: human_peer(nick, cm),
        account: account.map(str::to_string),
    }
}

/// The human-readable alias for a peer — the label a channel name is built
/// from. Each role gets a stable, deterministic spelling:
///
/// - `cc:<id>` → `cc-<id>`
/// - `mu:<daemon>` → `mu-<daemon>`
/// - `mu:<daemon>:<session>` → `mu-<daemon>-<session>`
/// - `human:<nick>` → `<nick>` (humans get no channel; see [`channel_for`])
/// - any other role → `<role>-<id>[-<sub>]`, empty levels dropped
pub fn peer_alias(peer: &PeerId) -> String {
    if let Some(nick) = peer.human_nick() {
        return nick.to_string();
    }
    let mut parts = vec![peer.role().to_string()];
    if !peer.id().is_empty() {
        parts.push(peer.id().to_string());
    }
    if let Some(sub) = peer.sub() {
        if !sub.is_empty() {
            parts.push(sub.to_string());
        }
    }
    sanitize(&parts.join("-"))
}

/// Replace characters that are illegal in an IRC channel name with `-`, so the
/// alias is always a legal channel body. Deterministic: reverse resolution
/// recomputes this exact function and compares.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_whitespace() || c.is_control() || matches!(c, ',' | ':' | '\u{7}') {
                '-'
            } else {
                c
            }
        })
        .collect()
}

/// Bytes of the stable hash tail used when the full channel name would exceed
/// `CHANNELLEN`. 8 hex chars = 32 bits — enough that ordinary rosters do not
/// collide, short enough to leave room for a readable head.
const HASH_LEN: usize = 8;

/// A short, stable hex hash of a peer id, for the channel tail. Deterministic:
/// the same peer id always hashes the same, so the same peer always maps to the
/// same channel.
fn hash_tail(peer: &PeerId) -> String {
    let digest = blake3::hash(peer.to_string().as_bytes());
    digest.to_hex()[..HASH_LEN].to_string()
}

/// The channel a peer maps to under `prefix`, kept within `channellen` bytes.
///
/// Returns `None` for a human operator: humans are never given a channel (they
/// are the gateway's own nick / private messages), so no human peer is ever
/// routed to one.
///
/// When `<prefix><alias>` fits, that is the channel. When it would exceed
/// `channellen`, the tail is replaced by `hash_tail` so the result stays
/// within budget AND is stable per peer. A pathologically small `channellen`
/// (no room for even the hash) still yields a deterministic, in-budget name —
/// two peers may then share it, which reverse resolution reports as ambiguous
/// rather than mis-routing.
pub fn channel_for(peer: &PeerId, prefix: &str, channellen: usize) -> Option<String> {
    if peer.is_human() {
        return None;
    }
    let alias = peer_alias(peer);
    let full = format!("{prefix}{alias}");
    if full.len() <= channellen {
        return Some(full);
    }
    // Budget for the body (everything after the prefix). If the prefix alone
    // already fills the budget, the channel is just the prefix, truncated to
    // fit — degenerate but deterministic.
    let avail = channellen.saturating_sub(prefix.len());
    if avail == 0 {
        return Some(truncate_bytes(prefix, channellen).to_string());
    }
    let hash = hash_tail(peer);
    let hash = &hash[..HASH_LEN.min(avail)];
    let keep = avail - hash.len();
    let head = truncate_bytes(&alias, keep);
    Some(format!("{prefix}{head}{hash}"))
}

/// Truncate to at most `max` bytes on a char boundary (the alias/prefix are
/// ASCII-heavy, but never split a multibyte scalar).
fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Replace characters that are illegal in an IRC NICK with `-`. The nick
/// grammar (RFC 2812, enforced by [`crate::config::validate_nick`]) admits
/// letters, digits, `-` and the nine specials `[ ] \\ ` _ ^ { | }`; everything
/// else — including the `:` and `.` a peer id carries, which the operator's
/// Ergo answers with `432 Erroneous nickname` — becomes `-`. Deterministic, so
/// the same peer always yields the same nick body.
fn sanitize_nick(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric()
                || c == '-'
                || matches!(c, '[' | ']' | '\\' | '`' | '_' | '^' | '{' | '|' | '}')
            {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The letter a puppet nick is prefixed with when the peer's role does not
/// start with one. The nick grammar allows a special (`[`, `_`, …) first, but
/// the design pins "first character must be a letter" so completion and
/// reading stay predictable; roles are letters today — this enforces rather
/// than assumes.
const NICK_LEAD: char = 'p';

/// The puppet nick body for a peer, before any length fitting:
/// `role-id[-sub]` through the NICK alphabet, led by a letter.
fn nick_alias(peer: &PeerId) -> String {
    let mut parts = vec![peer.role().to_string()];
    if !peer.id().is_empty() {
        parts.push(peer.id().to_string());
    }
    if let Some(sub) = peer.sub() {
        if !sub.is_empty() {
            parts.push(sub.to_string());
        }
    }
    let mut alias = sanitize_nick(&parts.join("-"));
    if !alias.starts_with(|c: char| c.is_ascii_alphabetic()) {
        alias.insert(0, NICK_LEAD);
    }
    alias
}

/// `alias` cut to leave room for the peer's [`hash_tail`], then the tail
/// appended, all within `nicklen` bytes. The same rule [`channel_for`] uses
/// for an over-long channel, so a nick and a channel that both had to be cut
/// carry the same 8-hex tail.
///
/// The alias's leading letter is always kept: a hex tail may start with a
/// digit, and a digit-led nick is a `432`, not a collision. So a `nicklen`
/// too small for even the tail yields the lead plus as much tail as fits —
/// deterministic and in-budget, and two peers may then collide, which
/// registration reports as `433` rather than mis-routing. `nicklen == 0` has
/// no valid nick at all and yields the empty string; the pool never
/// registers one (a server cannot advertise `NICKLEN=0`).
fn nick_tailed(peer: &PeerId, alias: &str, nicklen: usize) -> String {
    let hash = hash_tail(peer);
    // Room for the tail after the lead letter; the tail shrinks first.
    let keep = nicklen.saturating_sub(HASH_LEN).max(1.min(nicklen));
    let hash = &hash[..nicklen.saturating_sub(keep).min(HASH_LEN)];
    format!("{}{hash}", truncate_bytes(alias, keep))
}

/// The nick a puppet registers for `peer`, within `nicklen` bytes (the
/// server's advertised `NICKLEN`).
///
/// `None` for a human: humans keep their own names and never get a puppet.
/// Otherwise `role-id[-sub]` through the nick alphabet — `cc:c689911a` →
/// `cc-c689911a`, `mu:<daemon>:session-1` → `mu-<daemon>-session-1` — and when
/// that exceeds `nicklen` (a cc session id is a 36-char UUID, so `cc-<uuid>` is
/// 39 bytes against Ergo's 32), the id is cut and the peer's stable 8-hex hash
/// tail appended, so the result is inside budget and the same on every run.
/// Reverse resolution is never by parsing this back: see [`NickTable`].
pub fn nick_for(peer: &PeerId, nicklen: usize) -> Option<String> {
    if peer.is_human() {
        return None;
    }
    let alias = nick_alias(peer);
    if alias.len() <= nicklen {
        return Some(alias);
    }
    Some(nick_tailed(peer, &alias, nicklen))
}

/// The hash-tail form of a puppet nick, unconditionally: what a puppet
/// registers when its plain [`nick_for`] form collides with another peer's
/// under the server's folding, or is already held on the server (`433`, or a
/// human who joined first — humans are never contested). When the plain form
/// was itself already tailed (over budget) the two are identical, and a `433`
/// on this one means the agent stays channel-only for the session.
pub fn nick_for_tailed(peer: &PeerId, nicklen: usize) -> Option<String> {
    if peer.is_human() {
        return None;
    }
    Some(nick_tailed(peer, &nick_alias(peer), nicklen))
}

/// The relayed identity a channel operator speaks as through Ergo's
/// `RELAYMSG`: `<nick><separator>mu`, e.g. `cc-c689911a/mu` under the
/// advertised `draft/relaymsg=/`. A relayed identity is never a member of
/// anything and never appears in NAMES; it exists only on the lines it is
/// stamped on.
pub fn relayed_nick(nick: &str, separator: &str) -> String {
    format!("{nick}{separator}mu")
}

/// Why a nick could not be added to a [`NickTable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NickCollision {
    /// Another peer already holds a nick that folds equal under the table's
    /// casemapping — the server would treat the two as one identity.
    HeldBy(PeerId),
    /// This peer already holds a (different) nick; release it first.
    PeerHasNick(String),
}

/// `folded nick → peer` for the nicks the gateway itself holds (its own and
/// every puppet's). Reverse resolution of a puppet nick is by THIS table,
/// never by parsing: a nick not in it is a human or a stranger.
///
/// Like [`SelfNick`], every entry keeps the wire spelling and derives the
/// folded key from it, so a `CASEMAPPING` change re-derives from originals
/// rather than re-folding a folded value (which is lossy: `gw[` folded under
/// rfc1459 is `gw{` and would stay `gw{` under ascii, where the real nick now
/// folds to `gw[`). The set of folded keys is the gateway-owned nick set that
/// membership subtracts from human presence (increment 2a).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NickTable {
    cm: CaseMapping,
    /// folded nick → (wire spelling, peer)
    by_nick: HashMap<String, (String, PeerId)>,
    /// peer → folded nick
    by_peer: HashMap<PeerId, String>,
}

impl NickTable {
    /// An empty table folding under `cm`.
    pub fn new(cm: CaseMapping) -> Self {
        NickTable {
            cm,
            by_nick: HashMap::new(),
            by_peer: HashMap::new(),
        }
    }

    /// The casemapping the table currently folds under.
    pub fn casemapping(&self) -> CaseMapping {
        self.cm
    }

    /// Record that `peer` holds `nick` (the spelling the server accepted).
    /// Refused when another peer's nick folds equal, or when `peer` already
    /// holds a different nick — both are decisions for the pool, not silent
    /// overwrites. The same peer re-recording the same folded nick is fine and
    /// adopts the new spelling: a case-only `NICK` the server accepted is the
    /// same identity with a new wire form, and `nick_of` must report the form
    /// the server now uses.
    pub fn insert(&mut self, nick: &str, peer: PeerId) -> Result<(), NickCollision> {
        let folded = fold_nick(nick, self.cm);
        if let Some((orig, holder)) = self.by_nick.get_mut(&folded) {
            if *holder != peer {
                return Err(NickCollision::HeldBy(holder.clone()));
            }
            *orig = nick.to_string();
            return Ok(());
        }
        if let Some(held) = self.by_peer.get(&peer) {
            if *held != folded {
                return Err(NickCollision::PeerHasNick(
                    self.by_nick
                        .get(held)
                        .map(|(orig, _)| orig.clone())
                        .unwrap_or_else(|| held.clone()),
                ));
            }
        }
        self.by_peer.insert(peer.clone(), folded.clone());
        self.by_nick.insert(folded, (nick.to_string(), peer));
        Ok(())
    }

    /// Forget the nick `peer` holds, if any. Returns the wire spelling that was
    /// released.
    pub fn remove_peer(&mut self, peer: &PeerId) -> Option<String> {
        let folded = self.by_peer.remove(peer)?;
        self.by_nick.remove(&folded).map(|(orig, _)| orig)
    }

    /// The peer holding `nick` (folded under the table's casemapping), if the
    /// gateway holds it at all. `None` means a human or a stranger.
    pub fn resolve(&self, nick: &str) -> Option<&PeerId> {
        self.by_nick
            .get(&fold_nick(nick, self.cm))
            .map(|(_, peer)| peer)
    }

    /// The wire spelling of the nick `peer` holds, if any.
    pub fn nick_of(&self, peer: &PeerId) -> Option<&str> {
        self.by_peer
            .get(peer)
            .and_then(|folded| self.by_nick.get(folded))
            .map(|(orig, _)| orig.as_str())
    }

    /// Whether `nick` is one the gateway holds — the own-nick-SET guard.
    pub fn is_owned(&self, nick: &str) -> bool {
        self.by_nick.contains_key(&fold_nick(nick, self.cm))
    }

    /// Every held nick, folded — the gateway-owned nick set for membership.
    pub fn owned_folded(&self) -> impl Iterator<Item = &str> {
        self.by_nick.keys().map(String::as_str)
    }

    /// Number of nicks held.
    pub fn len(&self) -> usize {
        self.by_nick.len()
    }

    /// Whether no nick is held.
    pub fn is_empty(&self) -> bool {
        self.by_nick.is_empty()
    }

    /// The server changed `CASEMAPPING`: re-derive every folded key FROM THE
    /// WIRE SPELLING. Two entries whose spellings fold equal under the new
    /// mapping cannot both be kept; the one with the lexicographically later
    /// wire spelling is dropped and returned so the pool can re-register it
    /// under its tailed form. Deterministic, so two gateways reading the same
    /// change make the same choice.
    pub fn set_casemapping(&mut self, cm: CaseMapping) -> Vec<(String, PeerId)> {
        self.cm = cm;
        let mut entries: Vec<(String, PeerId)> = self.by_nick.drain().map(|(_, v)| v).collect();
        entries.sort();
        self.by_peer.clear();
        let mut dropped = Vec::new();
        for (orig, peer) in entries {
            if self.insert(&orig, peer.clone()).is_err() {
                dropped.push((orig, peer));
            }
        }
        dropped
    }
}

/// The outcome of resolving an IRC channel back to a mesh peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Exactly one current peer maps to the channel.
    Peer(PeerId),
    /// No current peer maps to it.
    Unknown,
    /// More than one current peer maps to it; the colliding peers are named so
    /// the operator can be told, and no destination is chosen.
    Ambiguous(Vec<PeerId>),
}

/// Resolve an IRC channel to the mesh peer it maps to, against the *current*
/// discovered peers `current` (never a stored roster). A peer matches when
/// [`channel_for`] recomputes to a channel the server would treat as the SAME
/// name — both sides are folded under `cm` before comparison, because a server
/// echoes back whatever case the sender typed and considers `#CC-abc` and
/// `#cc-abc` one channel (under rfc1459, `#a[b]` and `#a{b}` too). Humans never
/// match (they have no channel). Zero matches ⇒ [`Resolved::Unknown`]; one ⇒
/// [`Resolved::Peer`]; more than one ⇒ [`Resolved::Ambiguous`] — which now
/// includes peers whose channels collide only once folded, since the server
/// cannot tell those apart either.
pub fn resolve_channel(
    channel: &str,
    current: &[PeerId],
    prefix: &str,
    channellen: usize,
    cm: CaseMapping,
) -> Resolved {
    let want = fold_name(channel, cm);
    let mut hits: Vec<PeerId> = current
        .iter()
        .filter(|p| channel_for(p, prefix, channellen).is_some_and(|c| fold_name(&c, cm) == want))
        .cloned()
        .collect();
    match hits.len() {
        0 => Resolved::Unknown,
        1 => Resolved::Peer(hits.pop().expect("len checked")),
        _ => Resolved::Ambiguous(hits),
    }
}
