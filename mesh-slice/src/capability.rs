//! In-band capabilities (mu-wxc4 N12): a work request carries a biscuit
//! token that grants exactly the right the command needs; the service
//! verifies it before doing any work.
//!
//! This is the *fine-grained, per-request* tier. Biscuit is chosen because
//! it is Rust-native, offline-verifiable (the service needs only the issuer
//! public key, no callback), and **attenuable** — a holder can append checks
//! to narrow a grant before delegating it down a work chain, which is the
//! delegation the operator wants without minting a new token each hop. The
//! *coarse* tier (which subjects an agent may reach at all) is NATS's own
//! decentralized JWT/NKey authz at the connection, layered beneath this.

use biscuit_auth::macros::{authorizer, biscuit};
use biscuit_auth::{Biscuit, KeyPair, PublicKey};

/// Mint a capability granting a single right (e.g. `"code_recall"`), signed
/// by the issuer root key. Real deployments issue these from an authority
/// service; the slice mints them client-side against a shared root so the
/// verify path is exercised end to end.
pub fn mint(root: &KeyPair, right: &str) -> anyhow::Result<Vec<u8>> {
    let token = biscuit!(r#"right({r});"#, r = right).build(root)?;
    Ok(token.to_vec()?)
}

/// Does this token authorize `required_right`, verified against `issuer`?
///
/// Two independent gates, both must pass: (1) the signature verifies against
/// the issuer public key (`Biscuit::from`); (2) the datalog authorizer's
/// `allow` fires (the token carries the right). Any failure — bad signature,
/// missing right, malformed bytes, empty capability — returns `false`. There
/// is no fail-open path.
pub fn authorizes(token_bytes: &[u8], issuer: PublicKey, required_right: &str) -> bool {
    if token_bytes.is_empty() {
        return false;
    }
    let Ok(token) = Biscuit::from(token_bytes, issuer) else {
        return false;
    };
    let Ok(mut authz) = authorizer!(r#"allow if right({r});"#, r = required_right).build(&token)
    else {
        return false;
    };
    authz.authorize().is_ok()
}

/// mu-wxc4: the shared authorization gate — does an [`Envelope`]'s capability
/// grant the right its command requires, verified against `issuer`? Every
/// service uses this one check regardless of which command enum it serves.
///
/// [`Envelope`]: crate::contract::Envelope
pub fn authorize_envelope<C: crate::contract::MeshCommand>(
    env: &crate::contract::Envelope<C>,
    issuer: biscuit_auth::PublicKey,
) -> bool {
    authorizes(&env.capability, issuer, env.command.required_right())
}

#[cfg(test)]
mod tests {
    use super::*;
    use biscuit_auth::KeyPair;

    #[test]
    fn grants_only_the_minted_right() {
        let root = KeyPair::new();
        let token = mint(&root, "code_recall").expect("mint");
        // right present, correct issuer → authorized
        assert!(authorizes(&token, root.public(), "code_recall"));
        // a DIFFERENT right the token doesn't carry → refused (attenuation
        // point: a code_recall grant does not authorize code_status)
        assert!(!authorizes(&token, root.public(), "code_status"));
    }

    #[test]
    fn refuses_wrong_issuer_and_empty() {
        let root = KeyPair::new();
        let rogue = KeyPair::new();
        let token = mint(&root, "code_recall").expect("mint");
        // signature verifies against the ISSUER only, not any key
        assert!(!authorizes(&token, rogue.public(), "code_recall"));
        // an empty/absent capability is never authorized (no fail-open)
        assert!(!authorizes(&[], root.public(), "code_recall"));
        assert!(!authorizes(b"garbage", root.public(), "code_recall"));
    }
}
