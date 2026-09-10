//! Pure mesh↔IRC name mapping: nick folding, human identity, role aliases,
//! deterministic channel names, and reverse resolution.
//!
//! Everything here is a pure function of its arguments. Reverse resolution in
//! particular takes the *current* discovered-peer snapshot as a parameter — it
//! never consults a stored roster — so a changing mesh yields changing results
//! with no hidden state (the gateway's "process state is disposable" rule).

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
/// `channellen`, the tail is replaced by [`hash_tail`] so the result stays
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
