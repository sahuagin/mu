//! Activation of a *granted* skill: a skill whose tool is gated on a named
//! grant (mu-aws-mi2-18xx1.4; the generic successor of the `aws-recon`
//! activation of spec mu-039).
//!
//! Activating such a skill does three things, in order, and records them in
//! the rope so the activation is auditable from the session log alone:
//!
//! 1. builds the capability *request* the session needs — the tool in
//!    `allowed_tools` and the grants the operator is handing over;
//! 2. pins a `SkillActivation` span naming the skill, the requested grants,
//!    the catalog digest the runner tool was built against, and the tool
//!    schemas it exposes;
//! 3. registers the tool, which pins its `ToolSchema` span.
//!
//! The function never resolves a grant: the catalog is opaque to mu. What it
//! does check is coherence — the tool must be grant-gated, and the grant it
//! requires must be among the grants being activated, otherwise the
//! activation would produce a tool the session could never dispatch.

use std::collections::HashSet;
use std::sync::Arc;

use mu_core::agent::{Tool, ToolSpec};
use mu_core::capability::{Capability, DuplicateGrantError, Grant};
use mu_core::context::{RetainedRope, RetentionClass, Span, SpanKind};
use mu_core::skill::{Skill, SkillError, SkillManager};
use mu_core::tool_registry::ToolRegistry;
use serde_json::json;

#[derive(Debug, Clone, PartialEq)]
pub struct GrantedSkillActivation {
    pub skill: Skill,
    pub capability_request: Capability,
    pub tool_spec: ToolSpec,
}

#[derive(Debug, thiserror::Error)]
pub enum GrantedSkillError {
    #[error("tool `{0}` declares no required grant; a granted skill's tool must be grant-gated")]
    ToolNotGrantGated(String),
    #[error("tool `{tool}` requires grant `{grant}`, which this activation does not request")]
    GrantNotRequested { tool: String, grant: String },
    #[error(transparent)]
    DuplicateGrant(#[from] DuplicateGrantError),
    #[error(transparent)]
    Skill(#[from] SkillError),
}

/// Register and activate `skill_id` with `tool`, pinning the activation span
/// and the tool schema into `rope`. Returns the capability request the
/// session must be granted for the tool to dispatch.
pub fn activate_granted_skill(
    skill_id: &str,
    tool: Arc<dyn Tool>,
    grants: Vec<Grant>,
    catalog_digest: Option<&str>,
    skill_manager: &mut SkillManager,
    tool_registry: &mut ToolRegistry,
    rope: &mut RetainedRope,
) -> Result<Capability, GrantedSkillError> {
    let activation = build_activation(skill_id, tool.spec(), grants, catalog_digest)?;
    // Refuse BEFORE touching the catalog: `register` replaces the entry, so
    // registering first would leave the catalog describing an activation
    // the rope and the tool registry never saw.
    if skill_manager.is_active(skill_id) {
        return Err(SkillError::AlreadyActive(skill_id.to_owned()).into());
    }
    skill_manager.register(activation.skill);
    skill_manager.activate(skill_id, rope)?;
    tool_registry.register(tool, rope);
    Ok(activation.capability_request)
}

/// The pure half of [`activate_granted_skill`]: validates coherence and
/// builds the skill, its activation span and the capability request.
pub fn build_activation(
    skill_id: &str,
    tool_spec: ToolSpec,
    grants: Vec<Grant>,
    catalog_digest: Option<&str>,
) -> Result<GrantedSkillActivation, GrantedSkillError> {
    let required = tool_spec
        .policy
        .required_grant
        .clone()
        .ok_or_else(|| GrantedSkillError::ToolNotGrantGated(tool_spec.name.clone()))?;
    let grants = Grant::try_from_iter(grants)?;
    if !grants.iter().any(|g| g.name == required) {
        return Err(GrantedSkillError::GrantNotRequested {
            tool: tool_spec.name.clone(),
            grant: required,
        });
    }

    let capability_request = Capability {
        allowed_tools: Some(HashSet::from([tool_spec.name.clone()])),
        grants: grants.clone(),
        ..Default::default()
    };
    let span = activation_span(skill_id, &grants, catalog_digest, &tool_spec);
    let skill = Skill::new(skill_id, vec![span]);

    Ok(GrantedSkillActivation {
        skill,
        capability_request,
        tool_spec,
    })
}

fn activation_span(
    skill_id: &str,
    grants: &HashSet<Grant>,
    catalog_digest: Option<&str>,
    tool_spec: &ToolSpec,
) -> Span {
    let mut names: Vec<&str> = grants.iter().map(|g| g.name.as_str()).collect();
    names.sort_unstable();
    let requested: Vec<serde_json::Value> = names
        .iter()
        .map(|name| {
            let grant = grants
                .iter()
                .find(|g| g.name == *name)
                .expect("name from set");
            match &grant.policy {
                None => json!({ "name": name }),
                Some(policy) => json!({ "name": name, "policy": policy }),
            }
        })
        .collect();
    let content = json!({
        "kind": "skill_activated",
        "skill_id": skill_id,
        "capability_request": {
            "allowed_tools": [tool_spec.name],
            "grants": requested,
        },
        "catalog": { "digest": catalog_digest },
        "tool_schemas": [tool_spec.name],
        "required_grant": tool_spec.policy.required_grant,
        "audit": {
            "catalog_digest": catalog_digest,
        }
    });
    Span::new(
        format!("skill:{skill_id}:activation"),
        SpanKind::SkillActivation,
        serde_json::to_string_pretty(&content).expect("json serialization cannot fail"),
        RetentionClass::Pinned,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_core::agent::{PermissionLevel, RetryPolicy, SideEffects, ToolPolicy, ToolResult};
    use mu_core::context::RopeEvent;
    use serde_json::{json, Value};
    use std::future::Future;
    use std::pin::Pin;
    use tokio::sync::oneshot;

    /// A stand-in grant-gated tool; the runner tool itself is exercised in
    /// `tools::runner`.
    #[derive(Debug)]
    struct GatedTool {
        grant: Option<String>,
    }

    impl Tool for GatedTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new("infra_recon", "test", json!({"type": "object"})).with_policy(
                ToolPolicy {
                    side_effects: SideEffects::External,
                    permission: PermissionLevel::Allow,
                    retry: RetryPolicy::ModelDecides,
                    required_grant: self.grant.clone(),
                    idempotent: false,
                    ends_turn_on_success: false,
                },
            )
        }

        fn execute<'life0, 'async_trait>(
            &'life0 self,
            _arguments: Value,
            _cancel_rx: oneshot::Receiver<()>,
        ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async {
                ToolResult {
                    content: String::new(),
                    is_error: false,
                }
            })
        }
    }

    fn grant(name: &str) -> Grant {
        Grant {
            name: name.to_owned(),
            policy: None,
        }
    }

    fn gated() -> Arc<dyn Tool> {
        Arc::new(GatedTool {
            grant: Some("infra.scout.readonly".to_owned()),
        })
    }

    #[test]
    fn build_activation_returns_request_and_pinned_span() {
        let activation = build_activation(
            "infra-recon",
            gated().spec(),
            vec![grant("infra.scout.readonly"), grant("infra.audit.read")],
            Some("sha256:test"),
        )
        .expect("build ok");

        assert_eq!(activation.skill.id, "infra-recon");
        assert_eq!(activation.skill.spans.len(), 1);
        assert_eq!(activation.skill.spans[0].kind(), &SpanKind::SkillActivation);
        assert_eq!(
            activation.skill.spans[0].retention(),
            RetentionClass::Pinned
        );
        assert_eq!(
            activation.capability_request.allowed_tools,
            Some(HashSet::from(["infra_recon".to_owned()]))
        );
        assert_eq!(activation.capability_request.grants.len(), 2);
        assert!(activation
            .capability_request
            .grants
            .contains(&grant("infra.scout.readonly")));

        let content: Value =
            serde_json::from_str(activation.skill.spans[0].content()).expect("activation json");
        assert_eq!(content["kind"], "skill_activated");
        assert_eq!(content["skill_id"], "infra-recon");
        assert_eq!(
            content["capability_request"]["grants"],
            json!([{"name": "infra.audit.read"}, {"name": "infra.scout.readonly"}])
        );
        assert_eq!(content["catalog"]["digest"], "sha256:test");
        assert_eq!(content["required_grant"], "infra.scout.readonly");
        assert_eq!(content["tool_schemas"], json!(["infra_recon"]));
    }

    #[test]
    fn activation_registers_skill_and_tool_schema_spans() {
        let mut skill_manager = SkillManager::new();
        let mut tool_registry = ToolRegistry::new();
        let mut rope = RetainedRope::new();

        let cap = activate_granted_skill(
            "infra-recon",
            gated(),
            vec![grant("infra.scout.readonly")],
            None,
            &mut skill_manager,
            &mut tool_registry,
            &mut rope,
        )
        .expect("activate ok");

        assert!(skill_manager.is_active("infra-recon"));
        assert!(tool_registry.get("infra_recon").is_some());
        assert_eq!(rope.len(), 2);
        assert_eq!(rope.spans()[0].kind(), &SpanKind::SkillActivation);
        assert_eq!(rope.spans()[0].id(), "skill:infra-recon:activation");
        assert_eq!(rope.spans()[1].kind(), &SpanKind::ToolSchema);
        assert_eq!(rope.spans()[1].id(), "tool-schema:infra_recon");
        assert_eq!(tool_registry.attenuated_names(&cap), vec!["infra_recon"]);
        assert!(matches!(
            rope.events()[0],
            RopeEvent::SkillActivated { ref skill_id, .. } if skill_id == "infra-recon"
        ));
        assert!(matches!(
            rope.events()[1],
            RopeEvent::ToolSchemaRegistered { ref tool_name, .. } if tool_name == "infra_recon"
        ));
        let content: Value =
            serde_json::from_str(rope.spans()[0].content()).expect("activation json");
        assert!(content["catalog"]["digest"].is_null());
    }

    /// Re-activating an active skill is refused and changes nothing: the
    /// catalog, the rope and the tool registry keep the first activation.
    #[test]
    fn reactivation_is_refused_without_mutating_the_catalog() {
        let mut skill_manager = SkillManager::new();
        let mut tool_registry = ToolRegistry::new();
        let mut rope = RetainedRope::new();
        activate_granted_skill(
            "infra-recon",
            gated(),
            vec![grant("infra.scout.readonly")],
            Some("sha256:first"),
            &mut skill_manager,
            &mut tool_registry,
            &mut rope,
        )
        .expect("first activation");

        let err = activate_granted_skill(
            "infra-recon",
            gated(),
            vec![grant("infra.scout.readonly"), grant("infra.audit.read")],
            Some("sha256:second"),
            &mut skill_manager,
            &mut tool_registry,
            &mut rope,
        )
        .expect_err("second activation must fail");
        assert!(matches!(
            err,
            GrantedSkillError::Skill(SkillError::AlreadyActive(ref id)) if id == "infra-recon"
        ));
        assert_eq!(rope.len(), 2);
        let content: Value =
            serde_json::from_str(rope.spans()[0].content()).expect("activation json");
        assert_eq!(content["catalog"]["digest"], "sha256:first");
    }

    #[test]
    fn ungated_tool_is_refused() {
        let tool: Arc<dyn Tool> = Arc::new(GatedTool { grant: None });
        let err = build_activation("x", tool.spec(), vec![grant("infra.scout.readonly")], None)
            .expect_err("fails");
        assert!(matches!(err, GrantedSkillError::ToolNotGrantGated(name) if name == "infra_recon"));
    }

    #[test]
    fn tool_whose_grant_is_not_requested_is_refused() {
        let err = build_activation("x", gated().spec(), vec![grant("infra.audit.read")], None)
            .expect_err("fails");
        assert!(matches!(
            err,
            GrantedSkillError::GrantNotRequested { tool, grant }
                if tool == "infra_recon" && grant == "infra.scout.readonly"
        ));
    }

    #[test]
    fn conflicting_duplicate_grants_fail_closed() {
        let err = build_activation(
            "x",
            gated().spec(),
            vec![
                grant("infra.scout.readonly"),
                Grant {
                    name: "infra.scout.readonly".to_owned(),
                    policy: Some(json!({"narrow": true})),
                },
            ],
            None,
        )
        .expect_err("fails");
        assert!(matches!(err, GrantedSkillError::DuplicateGrant(_)));
    }
}
