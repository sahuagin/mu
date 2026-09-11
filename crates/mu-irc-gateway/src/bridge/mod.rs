//! The bridge: the one place the tested capability modules are actually run
//! against a live IRC server and a live mesh.
//!
//! Every decision here belongs to a module that was reviewed offline — the
//! [`adapter`](crate::adapter) registers, [`membership`](crate::membership)
//! tracks who is in which channel and which humans to front,
//! [`routing`](crate::routing) decides the mesh→IRC output,
//! [`outbound`](crate::outbound) decides the IRC→mesh publishes. The bridge
//! opens the sockets, runs the timers, executes the effects, and decides nothing
//! else.
//!
//! **Shape.** One task per *input*, one owner for the *state*, split along the
//! seam the two connections already fail along:
//!
//! - [`mesh_side`] is everything that outlives one IRC connection — the mesh
//!   subscriptions behind an ingress gate, the ordered presence worker, the
//!   ordered per-session publish worker, the `$SRV` discovery sweep, and the
//!   watcher that tracks the NATS connection in every phase;
//! - the session half is one IRC connection — the reader, registration, and ONE
//!   select loop that owns membership, routing, the outbound loop guards, the
//!   reconciler and the socket's write half, with the reconnect backoff around
//!   it.
//!
//! Both directions mutate the same membership and routing state, so a second
//! state-owning task would need a lock around all of it and would buy nothing.
//! The tasks exist exactly where an *await* would otherwise stall the mirror.
//!
//! **Nothing is replayed across a reconnect, and nothing is queued for one.**
//! On the IRC side the connection's queues die with it. On the mesh side the
//! subscriptions stay up (dropping them would lose the refusal detection that
//! tells an operator the observer is not watching), but they feed
//! [`mesh_side::ingress_gate`], which drops every event arriving while no
//! session is registered rather than retaining bodies for an outage's duration.
//! The IRC→mesh direction is the mirror image: its queue is bounded, created per
//! session and cancelled with it, emptied if the mesh goes down under it, and
//! while NATS is down a typed line is refused to the human's face instead of
//! being held. Membership, routing memory, the notice suppression and both
//! loop-guard windows are rebuilt empty, from fresh NAMES and fresh discovery.
//!
//! **Secret-safe and body-free.** No log line in either half carries a message
//! body or a credential — not even at `debug`. Bodies exist only inside a
//! [`RouteDecision`](crate::routing::RouteDecision) on its way to the socket;
//! diagnostics name classes, counts and peer ids.

pub mod mesh_side;
