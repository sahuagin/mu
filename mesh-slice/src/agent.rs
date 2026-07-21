//! C&C: agents as first-class mesh participants (mu-wxc4). The operator's
//! three uses:
//!   1. **who is available** — presence via NATS Micro `$SRV` discovery.
//!   2. **work with another agent** — a directed message (DM).
//!   3. **launch a team** — join a named team and multicast to its members.
//!
//! All inbound traffic (DMs and team messages) arrives on ONE event stream,
//! delivered **event-driven fire-and-forget**: a sender publishes and does
//! not block on a reply; the recipient receives an [`InboundEvent`]. This is
//! the dialogue/coordination use done natively over the mesh — what PR #492
//! bolted onto MCP. Every message carries an in-band capability the receiver
//! verifies before accepting; an unauthorized message is dropped, never
//! delivered.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use biscuit_auth::{KeyPair, PublicKey};
use bytes::Bytes;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use ulid::Ulid;

use crate::capability;
use crate::contract::{Envelope, MeshCommand};

/// An agent's C&C command set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentCommand {
    /// A directed message from `from`.
    Dm { from: String, body: String },
    /// A message multicast to a `team` by `from`.
    TeamMsg {
        from: String,
        team: String,
        body: String,
    },
}

impl MeshCommand for AgentCommand {
    fn required_right(&self) -> &'static str {
        match self {
            AgentCommand::Dm { .. } => "agent_dm",
            AgentCommand::TeamMsg { .. } => "team_msg",
        }
    }
}

/// One inbound event on an agent's single event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundEvent {
    Dm {
        from: String,
        body: String,
    },
    Team {
        team: String,
        from: String,
        body: String,
    },
}

const PRESENCE_PREFIX: &str = "agent_";

fn inbox_subject(agent_id: &str) -> String {
    format!("mu.agent.{agent_id}.dm")
}

fn team_subject(team: &str) -> String {
    format!("mu.team.{team}")
}

/// A mesh agent: discoverable (presence), sends DMs and team multicasts,
/// receives both as events. Holds the presence service + subscription tasks;
/// all live as long as the agent does.
pub struct Agent {
    id: String,
    client: async_nats::Client,
    /// Mints capabilities for this agent's outbound messages. In a real fleet
    /// the grant is issued by an authority; the slice shares one root so the
    /// verify path runs end to end.
    root: Arc<KeyPair>,
    /// Verifies inbound capabilities (both the DM inbox and any joined team).
    issuer: PublicKey,
    _presence: async_nats::service::Service,
    /// Cloned to feed each new subscription's forwarder into the single
    /// event stream.
    inbox_tx: mpsc::UnboundedSender<InboundEvent>,
    inbox_rx: mpsc::UnboundedReceiver<InboundEvent>,
    /// Teams already subscribed — makes [`Agent::join_team`] idempotent
    /// (re-joining must NOT add a second subscriber, which would double
    /// every team delivery; review 2026-07-21).
    joined_teams: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Forwarder tasks (DM inbox + one per joined team), aborted on drop so
    /// a dropped Agent releases its subscriptions promptly instead of
    /// lingering until the next message fails to send (review 2026-07-21).
    forwarders: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for Agent {
    fn drop(&mut self) {
        for h in self.forwarders.lock().expect("forwarders").drain(..) {
            h.abort();
        }
    }
}

impl Agent {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Join the mesh as `id`: register presence and start receiving DMs.
    /// `issuer` is the key inbound capabilities are verified against.
    pub async fn join(
        client: async_nats::Client,
        id: &str,
        root: Arc<KeyPair>,
        issuer: PublicKey,
    ) -> Result<Self> {
        use async_nats::service::ServiceExt;

        // Presence: a Micro service named `agent_<id>` makes this agent show
        // up in `$SRV` discovery. Registration IS the presence signal.
        let presence = client
            .service_builder()
            .description("mu agent (C&C presence)")
            .start(format!("{PRESENCE_PREFIX}{id}"), "0.1.0")
            .await
            .map_err(|e| anyhow!("presence register: {e}"))?;

        let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();

        // DM inbox: a plain subscription (event-driven receive).
        let dm_sub = client
            .subscribe(inbox_subject(id))
            .await
            .map_err(|e| anyhow!("inbox subscribe: {e}"))?;
        client
            .flush()
            .await
            .map_err(|e| anyhow!("flush after join: {e}"))?;
        let dm_forwarder = spawn_forwarder(dm_sub, issuer, inbox_tx.clone());

        Ok(Self {
            id: id.to_string(),
            client,
            root,
            issuer,
            _presence: presence,
            inbox_tx,
            inbox_rx,
            joined_teams: std::sync::Mutex::new(std::collections::HashSet::new()),
            forwarders: std::sync::Mutex::new(vec![dm_forwarder]),
        })
    }

    /// Who is available (use 1): live agent ids over NATS Micro `$SRV.PING` —
    /// no roster we maintain, no ip:port. Collects responders for a short
    /// window and strips the presence prefix.
    pub async fn who(&self) -> Result<Vec<String>> {
        let inbox = self.client.new_inbox();
        let mut sub = self
            .client
            .subscribe(inbox.clone())
            .await
            .map_err(|e| anyhow!("who inbox: {e}"))?;
        self.client
            .publish_with_reply("$SRV.PING", inbox, Bytes::new())
            .await
            .map_err(|e| anyhow!("who ping: {e}"))?;
        self.client
            .flush()
            .await
            .map_err(|e| anyhow!("who flush: {e}"))?;

        let mut agents = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(300);
        while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, sub.next()).await {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&msg.payload) {
                if let Some(name) = v.get("name").and_then(|n| n.as_str()) {
                    if let Some(agent) = name.strip_prefix(PRESENCE_PREFIX) {
                        agents.push(agent.to_string());
                    }
                }
            }
        }
        agents.sort();
        agents.dedup();
        Ok(agents)
    }

    /// Work with another agent (use 2): DM `body` to agent `to`. Fire-and-
    /// forget; the recipient receives an [`InboundEvent::Dm`].
    pub async fn dm(&self, to: &str, body: &str) -> Result<()> {
        self.publish(
            inbox_subject(to),
            AgentCommand::Dm {
                from: self.id.clone(),
                body: body.to_string(),
            },
        )
        .await
    }

    /// Launch/join a team (use 3): subscribe to the team's channel so this
    /// agent receives its multicasts as [`InboundEvent::Team`]. Idempotent:
    /// re-joining a team this agent already subscribed is a no-op (a second
    /// subscription would double every delivery).
    pub async fn join_team(&self, team: &str) -> Result<()> {
        // Claim membership atomically BEFORE subscribing: a concurrent
        // re-join sees the claim and returns instead of double-subscribing.
        // On subscribe failure the claim is RELEASED, so a transient error
        // does not poison future re-joins. (Closes both the sequential
        // error-poisoning and the concurrent double-join review findings.)
        if !self
            .joined_teams
            .lock()
            .expect("joined_teams")
            .insert(team.to_string())
        {
            return Ok(()); // already a member (or join in flight)
        }
        let sub = match self.client.subscribe(team_subject(team)).await {
            Ok(sub) => sub,
            Err(e) => {
                self.joined_teams.lock().expect("joined_teams").remove(team);
                return Err(anyhow!("team subscribe: {e}"));
            }
        };
        if let Err(e) = self.client.flush().await {
            // Release the claim on ANY post-claim failure — a kept claim
            // makes every future re-join a silent no-op (`sub` drops here,
            // unsubscribing client-side).
            self.joined_teams.lock().expect("joined_teams").remove(team);
            return Err(anyhow!("flush after team join: {e}"));
        }
        let handle = spawn_forwarder(sub, self.issuer, self.inbox_tx.clone());
        self.forwarders.lock().expect("forwarders").push(handle);
        Ok(())
    }

    /// Multicast `body` to every member of `team` (use 3). Fire-and-forget;
    /// each member receives an [`InboundEvent::Team`].
    pub async fn team_send(&self, team: &str, body: &str) -> Result<()> {
        self.publish(
            team_subject(team),
            AgentCommand::TeamMsg {
                from: self.id.clone(),
                team: team.to_string(),
                body: body.to_string(),
            },
        )
        .await
    }

    /// Await the next inbound event (DM or team message). `None` once the
    /// mesh connection is gone.
    pub async fn recv(&mut self) -> Option<InboundEvent> {
        self.inbox_rx.recv().await
    }

    /// Mint the command's capability, wrap it in the envelope, publish.
    async fn publish(&self, subject: String, command: AgentCommand) -> Result<()> {
        let capability = capability::mint(&self.root, command.required_right())?;
        let env = Envelope {
            id: Ulid::generate(),
            capability,
            command,
        };
        let payload = serde_json::to_vec(&env)?;
        self.client
            .publish(subject, payload.into())
            .await
            .map_err(|e| anyhow!("publish: {e}"))?;
        self.client
            .flush()
            .await
            .map_err(|e| anyhow!("publish flush: {e}"))?;
        Ok(())
    }
}

/// Drain a subscription: verify each message's capability, translate the
/// typed command to an [`InboundEvent`], forward accepted ones to the agent's
/// single event stream. Unauthorized or malformed messages are dropped.
fn spawn_forwarder(
    mut sub: async_nats::Subscriber,
    issuer: PublicKey,
    tx: mpsc::UnboundedSender<InboundEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(msg) = sub.next().await {
            let Ok(env) = serde_json::from_slice::<Envelope<AgentCommand>>(&msg.payload) else {
                continue;
            };
            if !capability::authorize_envelope(&env, issuer) {
                continue; // drop unauthorized; never deliver
            }
            let event = match env.command {
                AgentCommand::Dm { from, body } => InboundEvent::Dm { from, body },
                AgentCommand::TeamMsg { from, team, body } => {
                    InboundEvent::Team { team, from, body }
                }
            };
            if tx.send(event).is_err() {
                break; // agent dropped
            }
        }
    })
}
