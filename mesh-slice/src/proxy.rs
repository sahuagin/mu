//! The client-side abstraction (mu-wxc4, the operator's key point): mu code
//! calls THIS with the same shape it calls code_index today — `recall(...)`,
//! `status()` — and never sees NATS, envelopes, subjects, or capabilities.
//!
//! The proxy IS the abstraction: it interprets each call into "what to send,
//! on which channel" — here, a directed request/reply to the discovered
//! service subject, the reply relayed back synchronously as a typed value.
//! A broadcast-shaped command would instead fan-out publish; an inbound
//! stream would be a subscription. The caller's ergonomics don't change when
//! the bus does.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use biscuit_auth::KeyPair;
use ulid::Ulid;

use crate::capability;
use crate::contract::MeshCommand;
use crate::contract::{Command, CommandResult, Hit, Request, Response, StatusInfo};
use crate::service::{SERVICE_NAME, SERVICE_SUBJECT};

/// mu's handle to the code_index service. Holds only a NATS connection and
/// the key it mints request capabilities with — NO host:port. (In a real
/// deployment the capability is issued by an authority; the slice mints
/// client-side against a shared root so the end-to-end grant path runs.)
pub struct CodeIndexProxy {
    client: async_nats::Client,
    root: Arc<KeyPair>,
}

impl CodeIndexProxy {
    pub fn new(client: async_nats::Client, root: Arc<KeyPair>) -> Self {
        Self { client, root }
    }

    /// Discovery / observability (N1/N5): is the service live? Addressed by
    /// NAME over NATS Micro's `$SRV.PING`, not by endpoint. Proves the "find
    /// it by name, no hardcoded endpoint" property directly.
    pub async fn discover(&self) -> Result<bool> {
        let subject = format!("$SRV.PING.{SERVICE_NAME}");
        Ok(self
            .client
            .request(subject, bytes::Bytes::new())
            .await
            .is_ok())
    }

    /// `code_recall` — same call shape as today; returns typed hits.
    pub async fn recall(&self, query: &str, limit: Option<u32>) -> Result<Vec<Hit>> {
        self.recall_in(query, limit, None).await
    }

    /// `recall` targeting a specific index (`db`: serving-side name or
    /// absolute path); `None` = the service default.
    pub async fn recall_in(
        &self,
        query: &str,
        limit: Option<u32>,
        db: Option<String>,
    ) -> Result<Vec<Hit>> {
        match self
            .call(Command::CodeRecall {
                query: query.to_string(),
                limit,
                db,
            })
            .await?
        {
            CommandResult::CodeRecall(hits) => Ok(hits),
            CommandResult::Error(e) => Err(anyhow!("code_recall refused: {e}")),
            other => Err(anyhow!("unexpected result for recall: {other:?}")),
        }
    }

    /// `code_status` — same call shape as today.
    pub async fn status(&self) -> Result<StatusInfo> {
        match self.call(Command::CodeStatus).await? {
            CommandResult::CodeStatus(s) => Ok(s),
            CommandResult::Error(e) => Err(anyhow!("code_status refused: {e}")),
            other => Err(anyhow!("unexpected result for status: {other:?}")),
        }
    }

    /// The interpretation step: mint the capability the command needs, build
    /// the typed envelope, address the service by subject (never host:port),
    /// do the request/reply, decode the typed response. This is what the
    /// per-command public methods share; adding a command adds a method, not
    /// a new wire.
    async fn call(&self, command: Command) -> Result<CommandResult> {
        let capability = capability::mint(&self.root, command.required_right())?;
        let subject = format!("{SERVICE_SUBJECT}.{}", endpoint_of(&command));
        let request = Request {
            id: Ulid::generate(),
            capability,
            command,
        };
        let payload = serde_json::to_vec(&request)?;
        let reply = self
            .client
            .request(subject, payload.into())
            .await
            .map_err(|e| anyhow!("mesh request failed: {e}"))?;
        let response: Response = serde_json::from_slice(&reply.payload)?;
        if response.id != request.id {
            return Err(anyhow!(
                "correlation mismatch: sent {}, got {}",
                request.id,
                response.id
            ));
        }
        Ok(response.result)
    }
}

/// Which endpoint subject a command routes to.
fn endpoint_of(command: &Command) -> &'static str {
    match command {
        Command::CodeRecall { .. } => "recall",
        Command::CodeStatus => "status",
    }
}
