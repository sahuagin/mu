//! The dialogue/mesh peer identity, as a type (mu-uto4).
//!
//! A peer id is `role:id[:sub]` — `cc:<uuid>`, `mu:<daemon>`,
//! `mu:<daemon>:<session>`, `warden:<name>`. It used to be parsed and rebuilt
//! with `split(':')` and `format!` in nine places across three crates, which
//! is how the tree ended up with two disagreeing "canonical" spellings for the
//! same thing. Everything textual about a peer id now happens here.
//!
//! Parsing is TOTAL and lossless: any string is a `PeerId`, and `Display`
//! returns exactly what was parsed. That is deliberate — the peers table holds
//! whatever ids clients have ever presented, and a lossy round trip would
//! rewrite them. Validity is a separate question, asked with [`PeerId::as_mu`].

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The role prefix of a mu daemon or session.
pub const ROLE_MU: &str = "mu";

/// NATS Micro metadata key carrying a peer's identity, unmangled. Micro service
/// NAMES are restricted to `[A-Za-z0-9_-]`; metadata is not, so identity rides
/// here and the name stays a mere key (mu-b1lq).
pub const META_PEER_ID: &str = "peer_id";
/// Micro metadata key carrying the exact subject to publish to, so a sender
/// never re-derives the naming rule — a peer predating the hierarchy listens
/// only on its own flat subject.
pub const META_DM_SUBJECT: &str = "dm_subject";

/// A peer on the dialogue channel / agent mesh.
///
/// Construct with [`PeerId::parse`] (never fails) or the typed constructors;
/// render with `Display`. The separator lives in exactly one place, so changing
/// it is an edit here rather than a hunt through the tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerId {
    role: String,
    /// `None` when the id carried no separator at all, which is different from
    /// a separator with nothing after it (`"mu"` vs `"mu:"`). Collapsing the
    /// two would rewrite ids already sitting in the peers table.
    id: Option<String>,
    sub: Option<String>,
}

impl PeerId {
    /// Parse `role:id[:sub]`. Total: a string with no separator is all role
    /// (matching the long-standing "role is everything before the first `:`,
    /// or the whole thing" rule), and anything after the second separator
    /// stays in `sub`.
    pub fn parse(s: &str) -> Self {
        let mut parts = s.splitn(3, ':');
        let role = parts.next().unwrap_or_default().to_string();
        let id = parts.next().map(str::to_string);
        let sub = parts.next().map(str::to_string);
        Self { role, id, sub }
    }

    /// A daemon-level mu peer: `mu:<daemon>`.
    pub fn mu_daemon(daemon: impl Into<String>) -> Self {
        Self {
            role: ROLE_MU.to_string(),
            id: Some(daemon.into()),
            sub: None,
        }
    }

    /// A session-level mu peer: `mu:<daemon>:<session>`.
    pub fn mu_session(daemon: impl Into<String>, session: impl Into<String>) -> Self {
        Self {
            role: ROLE_MU.to_string(),
            id: Some(daemon.into()),
            sub: Some(session.into()),
        }
    }

    /// The role prefix — what `dialogue_peers` groups on.
    pub fn role(&self) -> &str {
        &self.role
    }

    /// The identity within the role: a daemon id for `mu`, a session uuid for
    /// `cc`. Empty when the id carried no separator.
    pub fn id(&self) -> &str {
        self.id.as_deref().unwrap_or_default()
    }

    /// The second level, when there is one: a session on a mu daemon.
    pub fn sub(&self) -> Option<&str> {
        self.sub.as_deref()
    }

    /// This peer as an addressable mu daemon + optional session, or `None` if
    /// it is not one.
    ///
    /// `None` covers every shape that was never addressable: a non-`mu` role,
    /// and the malformed `mu:`, `mu::x`, `mu:d:` — an empty level means the id
    /// names nothing, so it must not resolve to its parent.
    pub fn as_mu(&self) -> Option<MuPeer<'_>> {
        let daemon = self.id.as_deref().filter(|d| !d.is_empty())?;
        if self.role != ROLE_MU {
            return None;
        }
        match self.sub.as_deref() {
            Some("") => None,
            session => Some(MuPeer { daemon, session }),
        }
    }

    /// This peer's DM subject, with each level as a NATS subject token so the
    /// transport routes hierarchically — dispatch on the role, then the daemon,
    /// then the session. A daemon watches all of its sessions with
    /// `mu.agent.mu.<daemon>.*.dm` (mu-b1lq).
    pub fn dm_subject(&self) -> String {
        let mut s = String::from("mu.agent.");
        s.push_str(&self.role);
        if let Some(id) = self.id.as_deref().filter(|i| !i.is_empty()) {
            s.push('.');
            s.push_str(id);
        }
        if let Some(sub) = &self.sub {
            s.push('.');
            s.push_str(sub);
        }
        s.push_str(".dm");
        s
    }

    /// What this peer advertises about itself on `$SRV`: who it is, and where
    /// to reach it.
    pub fn mesh_metadata(&self) -> std::collections::HashMap<String, String> {
        std::collections::HashMap::from([
            (META_PEER_ID.to_string(), self.to_string()),
            (META_DM_SUBJECT.to_string(), self.dm_subject()),
        ])
    }

    /// A NATS Micro service name for this peer. Micro allows only
    /// `[A-Za-z0-9_-]`, so this is a registration KEY, never an identity —
    /// identity is advertised in service metadata, unmangled. Nothing parses
    /// the result.
    pub fn micro_name(&self) -> String {
        self.to_string()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }
}

/// An addressable mu peer: the daemon hosting it, and the session on it when
/// the id names one. Borrowed from the [`PeerId`] it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MuPeer<'a> {
    pub daemon: &'a str,
    pub session: Option<&'a str>,
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.role)?;
        // No separator in, no separator out.
        let Some(id) = &self.id else {
            return Ok(());
        };
        write!(f, ":{id}")?;
        if let Some(sub) = &self.sub {
            write!(f, ":{sub}")?;
        }
        Ok(())
    }
}

impl FromStr for PeerId {
    /// Parsing is total, so this never fails; `FromStr` exists for the callers
    /// that want `"…".parse()`.
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::parse(s))
    }
}

impl From<&str> for PeerId {
    fn from(s: &str) -> Self {
        Self::parse(s)
    }
}

impl Serialize for PeerId {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PeerId {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Ok(Self::parse(&s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every shape in use round-trips byte for byte. The peers table holds ids
    /// clients presented long ago; parsing must not rewrite them.
    #[test]
    fn parsing_is_total_and_round_trips() {
        for s in [
            "cc:17302f24-836a-4f82-a988-cb711338e6e7",
            "mu:100c058851c356a5",
            "mu:100c058851c356a5:session-2",
            "warden:sub_agent_3",
            "cc:deploy-test",
            // Degenerate shapes the store has really held.
            "foo",
            "",
            "mu:",
            "mu::session-1",
            "mu:daemon:",
        ] {
            assert_eq!(PeerId::parse(s).to_string(), s, "round trip for {s:?}");
        }
        // A third separator stays inside `sub` rather than being dropped.
        let p = PeerId::parse("mu:d:s:extra");
        assert_eq!(p.sub(), Some("s:extra"));
        assert_eq!(p.to_string(), "mu:d:s:extra");
    }

    /// The role rule the peers table has always used: everything before the
    /// first separator, or the whole string when there is none.
    #[test]
    fn role_matches_the_long_standing_rule() {
        assert_eq!(PeerId::parse("cc:abc").role(), "cc");
        assert_eq!(PeerId::parse("mu:d:s").role(), "mu");
        assert_eq!(PeerId::parse("foo").role(), "foo");
        assert_eq!(PeerId::parse("").role(), "");
    }

    /// `as_mu` reproduces exactly what the old `resolve_target` accepted, so
    /// nothing becomes addressable that was not before.
    #[test]
    fn only_well_formed_mu_ids_are_addressable() {
        let daemon = PeerId::parse("mu:100c058851c356a5");
        assert_eq!(
            daemon.as_mu(),
            Some(MuPeer {
                daemon: "100c058851c356a5",
                session: None
            })
        );
        let session = PeerId::parse("mu:100c058851c356a5:session-2");
        assert_eq!(
            session.as_mu(),
            Some(MuPeer {
                daemon: "100c058851c356a5",
                session: Some("session-2")
            })
        );
        // Not addressable: served by the gateway's own store, or malformed.
        for s in [
            "cc:abc",
            "warden:x",
            "mu:",
            "mu::session-1",
            "mu:daemon:",
            "",
        ] {
            assert_eq!(PeerId::parse(s).as_mu(), None, "{s:?} must not resolve");
        }
    }

    #[test]
    fn subjects_are_hierarchical() {
        assert_eq!(
            PeerId::parse("mu:100c058851c356a5:session-2").dm_subject(),
            "mu.agent.mu.100c058851c356a5.session-2.dm"
        );
        assert_eq!(
            PeerId::parse("mu:100c058851c356a5").dm_subject(),
            "mu.agent.mu.100c058851c356a5.dm"
        );
        assert_eq!(
            PeerId::parse("cc:deploy-test").dm_subject(),
            "mu.agent.cc.deploy-test.dm"
        );
        // A daemon's wildcard for its sessions is a prefix of its sessions'.
        let d = PeerId::mu_daemon("d");
        let s = PeerId::mu_session("d", "s1");
        assert!(s
            .dm_subject()
            .starts_with(d.dm_subject().trim_end_matches(".dm")));
    }

    #[test]
    fn micro_names_are_legal_for_every_shape() {
        for s in [
            "cc:17302f24-836a-4f82-a988-cb711338e6e7",
            "warden:sub_agent_3",
            "mu:100c058851c356a5:session-2",
        ] {
            let n = PeerId::parse(s).micro_name();
            assert!(
                n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{n} is not a legal NATS Micro service name"
            );
        }
    }

    #[test]
    fn mesh_metadata_advertises_identity_and_reachability() {
        let m = PeerId::mu_session("d", "session-1").mesh_metadata();
        assert_eq!(m[META_PEER_ID], "mu:d:session-1");
        assert_eq!(m[META_DM_SUBJECT], "mu.agent.mu.d.session-1.dm");
    }

    #[test]
    fn serde_uses_the_textual_form() {
        let p = PeerId::mu_session("d", "session-1");
        let j = serde_json::to_string(&p).unwrap();
        assert_eq!(j, "\"mu:d:session-1\"");
        assert_eq!(serde_json::from_str::<PeerId>(&j).unwrap(), p);
    }

    #[test]
    fn typed_constructors_agree_with_parsing() {
        assert_eq!(PeerId::mu_daemon("d"), PeerId::parse("mu:d"));
        assert_eq!(PeerId::mu_session("d", "s"), PeerId::parse("mu:d:s"));
    }
}
