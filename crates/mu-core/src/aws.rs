//! AWS capability catalog types.
//!
//! This module is the Mu-side bridge between [`AwsCapability`] grants
//! carried by a session and an operator-managed external catalog such as
//! `mu-aws-sandbox-infra/capabilities/aws.json`. It deliberately does
//! no AWS I/O and materializes no credentials; callers load JSON from a
//! trusted path, parse it into [`AwsCapabilityCatalog`], and resolve
//! capability names before handing execution to a broker/runner.
//!
//! [`AwsCapability`]: crate::capability::AwsCapability

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::capability::AwsCapability;

/// Operator-managed catalog mapping Mu AWS capability names to their
/// concrete AWS materialization metadata.
///
/// The shape matches the JSON catalog used by the AWS sandbox repo.
/// Unknown fields are tolerated for forward compatibility: the catalog
/// is owned outside `mu-core`, and Mu only needs the stable subset
/// required to answer "is this named capability materialized yet?".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwsCapabilityCatalog {
    /// Catalog schema version. Version 1 is the initial JSON shape.
    pub schema_version: u32,
    /// Default AWS region used by the catalog, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_region: Option<String>,
    /// Capability entries keyed by `AwsCapability::name`.
    #[serde(default)]
    pub capabilities: BTreeMap<String, AwsCapabilityCatalogEntry>,
}

impl AwsCapabilityCatalog {
    /// Resolve a capability by name. This does not require the entry
    /// to be materialized; planned entries are returned too.
    pub fn resolve(
        &self,
        capability: &AwsCapability,
    ) -> Result<&AwsCapabilityCatalogEntry, AwsCatalogError> {
        self.capabilities
            .get(&capability.name)
            .ok_or_else(|| AwsCatalogError::UnknownCapability {
                name: capability.name.clone(),
            })
    }

    /// Resolve a capability and require that it has concrete local/AWS
    /// materialization metadata.
    ///
    /// Planned catalog entries intentionally fail here. This is the
    /// fail-closed boundary between "Mu can name this future scope" and
    /// "a runner/broker may try to assume/use it now".
    pub fn resolve_materialized(
        &self,
        capability: &AwsCapability,
    ) -> Result<&AwsCapabilityCatalogEntry, AwsCatalogError> {
        let entry = self.resolve(capability)?;
        if !entry.is_materialized() {
            return Err(AwsCatalogError::CapabilityNotMaterialized {
                name: capability.name.clone(),
                status: entry.status.clone(),
            });
        }
        Ok(entry)
    }
}

/// One capability entry in an [`AwsCapabilityCatalog`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwsCapabilityCatalogEntry {
    /// Human-readable description for operator/auditor views.
    pub description: String,
    /// Local AWS profile the runner should select, when materialized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_profile: Option<String>,
    /// IAM role name, when materialized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_name: Option<String>,
    /// IAM role ARN, when materialized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_arn: Option<String>,
    /// Optional lifecycle status. `planned` means explicitly not
    /// materialized yet. Missing status is treated as active iff the
    /// profile and role ARN are present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Whether this capability is expected to permit mutations.
    #[serde(default)]
    pub mutation_allowed: bool,
    /// AWS managed policies attached to the role, if documented.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub managed_policies: Vec<String>,
    /// Inline policy names attached to the role, if documented.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inline_policies: Vec<String>,
    /// Optional service/action list for planned narrow capabilities.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target_services: Vec<String>,
    /// Free-form constraints block. The catalog owns the exact schema;
    /// Mu preserves it for audit/operator display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraints: Option<serde_json::Value>,
    /// Free-form smoke-test block. Preserved for operator tooling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smoke_tests: Option<serde_json::Value>,
    /// Additional operator notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl AwsCapabilityCatalogEntry {
    /// True iff this entry has enough concrete metadata for a broker or
    /// runner to try materializing it now.
    pub fn is_materialized(&self) -> bool {
        if self.status.as_deref() == Some("planned") {
            return false;
        }
        self.aws_profile.is_some() && self.role_arn.is_some()
    }
}

/// Fail-closed catalog resolution errors.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AwsCatalogError {
    /// The session held an AWS capability name absent from the catalog.
    #[error("unknown AWS capability: {name}")]
    UnknownCapability { name: String },
    /// The catalog knows the name, but it is only planned or lacks the
    /// profile/role ARN required for execution.
    #[error("AWS capability is not materialized: {name} (status: {status:?})")]
    CapabilityNotMaterialized {
        name: String,
        status: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aws(name: &str) -> AwsCapability {
        AwsCapability {
            name: name.to_string(),
            session_policy: None,
        }
    }

    fn sample_catalog() -> AwsCapabilityCatalog {
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "default_region": "us-east-1",
            "capabilities": {
                "aws.scout.readonly": {
                    "description": "Inventory and discovery without mutation.",
                    "aws_profile": "mu-readonly-scout",
                    "role_name": "mu-readonly-scout",
                    "role_arn": "arn:aws:iam::123456789012:role/mu-readonly-scout",
                    "mutation_allowed": false,
                    "managed_policies": ["arn:aws:iam::aws:policy/ReadOnlyAccess"],
                    "smoke_tests": {
                        "allowed": ["sts:GetCallerIdentity"],
                        "denied": ["iam:CreateRole"]
                    }
                },
                "aws.sandbox.build": {
                    "description": "Controlled sandbox mutation.",
                    "aws_profile": "mu-sandbox-builder",
                    "role_name": "mu-sandbox-builder",
                    "role_arn": "arn:aws:iam::123456789012:role/mu-sandbox-builder",
                    "mutation_allowed": true,
                    "inline_policies": ["mu-sandbox-builder"],
                    "constraints": {
                        "ssm_parameter_path_prefix": "/mu-sandbox/"
                    }
                },
                "aws.vpc.build": {
                    "description": "Future tagged VPC builder.",
                    "aws_profile": null,
                    "role_name": null,
                    "role_arn": null,
                    "status": "planned",
                    "mutation_allowed": true,
                    "target_services": ["ec2:CreateVpc"]
                }
            }
        }))
        .expect("sample catalog parses")
    }

    #[test]
    fn parses_catalog_and_resolves_materialized_capability() {
        let catalog = sample_catalog();
        let entry = catalog
            .resolve_materialized(&aws("aws.scout.readonly"))
            .expect("scout is materialized");

        assert_eq!(catalog.schema_version, 1);
        assert_eq!(catalog.default_region.as_deref(), Some("us-east-1"));
        assert_eq!(entry.aws_profile.as_deref(), Some("mu-readonly-scout"));
        assert_eq!(entry.role_name.as_deref(), Some("mu-readonly-scout"));
        assert_eq!(
            entry.role_arn.as_deref(),
            Some("arn:aws:iam::123456789012:role/mu-readonly-scout")
        );
        assert!(!entry.mutation_allowed);
        assert!(entry.is_materialized());
    }

    #[test]
    fn preserves_mutating_constraints_for_auditor_or_broker() {
        let catalog = sample_catalog();
        let entry = catalog
            .resolve_materialized(&aws("aws.sandbox.build"))
            .expect("sandbox build is materialized");

        assert!(entry.mutation_allowed);
        assert_eq!(entry.inline_policies, ["mu-sandbox-builder"]);
        assert_eq!(
            entry
                .constraints
                .as_ref()
                .and_then(|v| v.get("ssm_parameter_path_prefix"))
                .and_then(serde_json::Value::as_str),
            Some("/mu-sandbox/")
        );
    }

    #[test]
    fn planned_capability_resolves_but_fails_materialization() {
        let catalog = sample_catalog();
        let planned = aws("aws.vpc.build");

        assert_eq!(
            catalog
                .resolve(&planned)
                .expect("planned entry exists")
                .status
                .as_deref(),
            Some("planned")
        );
        assert_eq!(
            catalog.resolve_materialized(&planned),
            Err(AwsCatalogError::CapabilityNotMaterialized {
                name: "aws.vpc.build".to_string(),
                status: Some("planned".to_string()),
            })
        );
    }

    #[test]
    fn unknown_capability_fails_closed() {
        let catalog = sample_catalog();

        assert_eq!(
            catalog.resolve_materialized(&aws("aws.not.in.catalog")),
            Err(AwsCatalogError::UnknownCapability {
                name: "aws.not.in.catalog".to_string(),
            })
        );
    }
}
