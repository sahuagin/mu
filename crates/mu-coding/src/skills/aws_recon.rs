use std::collections::HashSet;
use std::sync::Arc;

use mu_core::agent::{Tool, ToolSpec};
use mu_core::aws::{AwsCapabilityCatalog, AwsCapabilityCatalogEntry, AwsCatalogError};
use mu_core::capability::{AwsCapability, Capability};
use mu_core::context::{RetainedRope, RetentionClass, Span, SpanKind};
use mu_core::skill::{Skill, SkillError, SkillManager};
use mu_core::tool_registry::ToolRegistry;
use serde_json::json;

const SKILL_ID: &str = "aws-recon";
const AWS_RECON_TOOL: &str = "aws_recon";
const REQUIRED_AWS_CAPABILITY: &str = "aws.scout.readonly";

#[derive(Debug, Clone, PartialEq)]
pub struct AwsReconSkillActivation {
    pub skill: Skill,
    pub capability_request: Capability,
    pub tool_spec: ToolSpec,
}

#[derive(Debug, thiserror::Error)]
pub enum AwsReconSkillError {
    #[error("aws-recon skill requires tool named aws_recon, got {0}")]
    WrongToolName(String),
    #[error("aws_recon tool must declare required AWS capability aws.scout.readonly")]
    MissingRequiredAwsCapability,
    #[error(transparent)]
    Catalog(#[from] AwsCatalogError),
    #[error("aws-recon requires a read-only AWS capability, but {0} allows mutation")]
    MutationAllowed(String),
    #[error(transparent)]
    Skill(#[from] SkillError),
}

pub fn activate_aws_recon_fixture_skill(
    tool: Arc<dyn Tool>,
    catalog: &AwsCapabilityCatalog,
    catalog_digest: impl Into<String>,
    skill_manager: &mut SkillManager,
    tool_registry: &mut ToolRegistry,
    rope: &mut RetainedRope,
) -> Result<Capability, AwsReconSkillError> {
    let activation = build_activation(tool.spec(), catalog, catalog_digest)?;
    skill_manager.register(activation.skill);
    skill_manager.activate(SKILL_ID, rope)?;
    tool_registry.register(tool, rope);
    Ok(activation.capability_request)
}

pub fn build_activation(
    tool_spec: ToolSpec,
    catalog: &AwsCapabilityCatalog,
    catalog_digest: impl Into<String>,
) -> Result<AwsReconSkillActivation, AwsReconSkillError> {
    validate_tool_spec(&tool_spec)?;

    let requested = AwsCapability {
        name: REQUIRED_AWS_CAPABILITY.to_owned(),
        session_policy: None,
    };
    let entry = catalog.resolve_materialized(&requested)?;
    if entry.mutation_allowed {
        return Err(AwsReconSkillError::MutationAllowed(requested.name));
    }

    let catalog_digest = catalog_digest.into();
    let capability_request = Capability {
        allowed_tools: Some(HashSet::from([AWS_RECON_TOOL.to_owned()])),
        aws: HashSet::from([requested.clone()]),
        ..Default::default()
    };
    let activation_span = activation_span(&requested, entry, &catalog_digest, &tool_spec);
    let skill = Skill::new(SKILL_ID, vec![activation_span]);

    Ok(AwsReconSkillActivation {
        skill,
        capability_request,
        tool_spec,
    })
}

fn validate_tool_spec(tool_spec: &ToolSpec) -> Result<(), AwsReconSkillError> {
    if tool_spec.name != AWS_RECON_TOOL {
        return Err(AwsReconSkillError::WrongToolName(tool_spec.name.clone()));
    }
    if tool_spec.policy.required_aws_capability.as_deref() != Some(REQUIRED_AWS_CAPABILITY) {
        return Err(AwsReconSkillError::MissingRequiredAwsCapability);
    }
    Ok(())
}

fn activation_span(
    requested: &AwsCapability,
    entry: &AwsCapabilityCatalogEntry,
    catalog_digest: &str,
    tool_spec: &ToolSpec,
) -> Span {
    let content = json!({
        "kind": "skill_activated",
        "skill_id": SKILL_ID,
        "capability_request": {
            "aws": [{ "name": requested.name }]
        },
        "catalog": {
            "schema_version": 1,
            "digest": catalog_digest,
        },
        "materialized_caps": [{
            "name": requested.name,
            "aws_profile": entry.aws_profile,
            "role_arn": entry.role_arn,
            "mutation_allowed": entry.mutation_allowed,
        }],
        "tool_schemas": [tool_spec.name],
        "runner": {
            "mode": "fixture",
            "live_aws": false,
            "subprocess": false,
        },
        "audit": {
            "mu_session_id": null,
            "tool_call_id": null,
            "catalog_digest": catalog_digest,
            "sts": null,
        }
    });

    Span::new(
        "skill:aws-recon:activation",
        SpanKind::SkillActivation,
        serde_json::to_string_pretty(&content).expect("json serialization cannot fail"),
        RetentionClass::Pinned,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AwsReconTool;
    use mu_core::context::RopeEvent;
    use serde_json::{json, Value};

    fn catalog_with(entries: Value) -> AwsCapabilityCatalog {
        serde_json::from_value(json!({
            "schema_version": 1,
            "default_region": "us-east-1",
            "capabilities": entries,
        }))
        .expect("catalog fixture parses")
    }

    fn readonly_catalog() -> AwsCapabilityCatalog {
        catalog_with(json!({
            "aws.scout.readonly": {
                "description": "Read-only scout.",
                "aws_profile": "mu-readonly-scout",
                "role_name": "mu-readonly-scout",
                "role_arn": "arn:aws:iam::123456789012:role/mu-readonly-scout",
                "mutation_allowed": false,
                "managed_policies": ["arn:aws:iam::aws:policy/ReadOnlyAccess"]
            }
        }))
    }

    fn aws_recon_tool(catalog: AwsCapabilityCatalog) -> Arc<dyn Tool> {
        Arc::new(AwsReconTool::new(
            catalog,
            "sha256:test",
            Some("fixture".into()),
        ))
    }

    #[test]
    fn build_activation_returns_capability_request_and_skill_span() {
        let catalog = readonly_catalog();
        let tool = aws_recon_tool(catalog.clone());
        let activation = build_activation(tool.spec(), &catalog, "sha256:test").expect("build ok");

        assert_eq!(activation.skill.id, "aws-recon");
        assert_eq!(activation.skill.spans.len(), 1);
        assert_eq!(activation.skill.spans[0].kind, SpanKind::SkillActivation);
        assert_eq!(activation.skill.spans[0].retention, RetentionClass::Pinned);

        assert_eq!(
            activation.capability_request.allowed_tools,
            Some(HashSet::from(["aws_recon".to_owned()]))
        );
        assert_eq!(activation.capability_request.aws.len(), 1);
        assert!(activation.capability_request.aws.contains(&AwsCapability {
            name: "aws.scout.readonly".to_owned(),
            session_policy: None,
        }));

        let content: Value =
            serde_json::from_str(&activation.skill.spans[0].content).expect("activation json");
        assert_eq!(content["kind"], "skill_activated");
        assert_eq!(content["skill_id"], "aws-recon");
        assert_eq!(
            content["capability_request"]["aws"][0]["name"],
            "aws.scout.readonly"
        );
        assert_eq!(content["catalog"]["digest"], "sha256:test");
        assert_eq!(
            content["materialized_caps"][0]["aws_profile"],
            "mu-readonly-scout"
        );
        assert_eq!(content["materialized_caps"][0]["mutation_allowed"], false);
        assert_eq!(content["tool_schemas"], json!(["aws_recon"]));
        assert_eq!(content["runner"]["mode"], "fixture");
        assert_eq!(content["runner"]["live_aws"], false);
    }

    #[test]
    fn activation_registers_skill_and_tool_schema_spans() {
        let catalog = readonly_catalog();
        let tool = aws_recon_tool(catalog.clone());
        let mut skill_manager = SkillManager::new();
        let mut tool_registry = ToolRegistry::new();
        let mut rope = RetainedRope::new();

        let cap = activate_aws_recon_fixture_skill(
            tool,
            &catalog,
            "sha256:test",
            &mut skill_manager,
            &mut tool_registry,
            &mut rope,
        )
        .expect("activate ok");

        assert!(skill_manager.is_active("aws-recon"));
        assert!(tool_registry.get("aws_recon").is_some());
        assert_eq!(rope.len(), 2);
        assert_eq!(rope.spans()[0].kind, SpanKind::SkillActivation);
        assert_eq!(rope.spans()[1].kind, SpanKind::ToolSchema);
        assert_eq!(rope.spans()[1].id, "tool-schema:aws_recon");
        assert_eq!(tool_registry.attenuated_names(&cap), vec!["aws_recon"]);

        assert!(matches!(
            rope.events()[0],
            RopeEvent::SkillActivated { ref skill_id, .. } if skill_id == "aws-recon"
        ));
        assert!(matches!(
            rope.events()[1],
            RopeEvent::ToolSchemaRegistered { ref tool_name, .. } if tool_name == "aws_recon"
        ));
    }

    #[test]
    fn catalog_miss_fails_closed_before_activation() {
        let catalog = catalog_with(json!({}));
        let tool = aws_recon_tool(readonly_catalog());
        let err = build_activation(tool.spec(), &catalog, "sha256:test").expect_err("fails");

        assert!(matches!(
            err,
            AwsReconSkillError::Catalog(AwsCatalogError::UnknownCapability { .. })
        ));
    }

    #[test]
    fn mutating_catalog_entry_is_refused() {
        let catalog = catalog_with(json!({
            "aws.scout.readonly": {
                "description": "Bad mutating scout.",
                "aws_profile": "mu-sandbox-builder",
                "role_name": "mu-sandbox-builder",
                "role_arn": "arn:aws:iam::123456789012:role/mu-sandbox-builder",
                "mutation_allowed": true
            }
        }));
        let tool = aws_recon_tool(readonly_catalog());
        let err = build_activation(tool.spec(), &catalog, "sha256:test").expect_err("fails");

        assert!(matches!(
            err,
            AwsReconSkillError::MutationAllowed(name) if name == "aws.scout.readonly"
        ));
    }

    #[test]
    fn wrong_tool_spec_is_refused() {
        let catalog = readonly_catalog();
        let spec = ToolSpec::new("read", "not aws recon", json!({"type":"object"}));
        let err = build_activation(spec, &catalog, "sha256:test").expect_err("fails");

        assert!(matches!(err, AwsReconSkillError::WrongToolName(name) if name == "read"));
    }
}
