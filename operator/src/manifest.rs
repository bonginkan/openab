use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level manifest that can be either OABService or OABFleet
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RawManifest {
    pub api_version: String,
    pub kind: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OABServiceManifest {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: Spec,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OABFleetManifest {
    pub api_version: String,
    pub kind: String,
    pub metadata: FleetMetadata,
    pub spec: FleetSpec,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct FleetMetadata {
    pub name: String,
    pub namespace: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetSpec {
    pub template: FleetTemplate,
    pub agents: Vec<AgentOverride>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FleetTemplate {
    pub image: String,
    #[serde(default)]
    pub resources: Option<Resources>,
    #[serde(default)]
    pub bootstrap_from: Option<String>,
    #[serde(default)]
    pub secrets: HashMap<String, String>,
    pub runtime: Runtime,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentOverride {
    pub name: String,
    pub config_from: String,
    #[serde(default)]
    pub resources: Option<Resources>,
    #[serde(default)]
    pub bootstrap_from: Option<String>,
    #[serde(default)]
    pub secrets: Option<HashMap<String, String>>,
    #[serde(default)]
    pub image: Option<String>,
}

impl OABFleetManifest {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.api_version != "oab.dev/v2" {
            anyhow::bail!("unsupported apiVersion: {} (expected oab.dev/v2)", self.api_version);
        }
        if self.kind != "OABFleet" {
            anyhow::bail!("unsupported kind: {}", self.kind);
        }
        if self.metadata.name.is_empty() {
            anyhow::bail!("metadata.name is required");
        }
        if self.spec.agents.is_empty() {
            anyhow::bail!("spec.agents must not be empty");
        }
        for agent in &self.spec.agents {
            if agent.name.is_empty() {
                anyhow::bail!("each agent must have a name");
            }
            if agent.config_from.is_empty() {
                anyhow::bail!("agent '{}': configFrom is required", agent.name);
            }
        }
        Ok(())
    }

    /// Expand fleet into individual OABService manifests
    pub fn expand(&self) -> Vec<OABServiceManifest> {
        self.spec.agents.iter().map(|agent| {
            let resources = agent.resources.clone()
                .or(self.spec.template.resources.clone())
                .unwrap_or(Resources { cpu: "256".into(), memory: "512".into() });
            let base_secrets = agent.secrets.clone()
                .unwrap_or_else(|| self.spec.template.secrets.clone());
            // Interpolate ${name} in secret values
            let secrets = base_secrets.into_iter().map(|(k, v)| {
                (k, v.replace("${name}", &agent.name))
            }).collect();

            OABServiceManifest {
                api_version: self.api_version.clone(),
                kind: "OABService".to_string(),
                metadata: Metadata {
                    name: agent.name.clone(),
                    namespace: self.metadata.namespace.clone(),
                    generation: 0,
                },
                spec: Spec {
                    image: agent.image.clone()
                        .unwrap_or_else(|| self.spec.template.image.clone()),
                    resources,
                    config_from: agent.config_from.replace("${name}", &agent.name),
                    bootstrap_from: agent.bootstrap_from.clone()
                        .or(self.spec.template.bootstrap_from.clone())
                        .map(|s| s.replace("${name}", &agent.name)),
                    secrets,
                    runtime: self.spec.template.runtime.clone(),
                },
            }
        }).collect()
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Metadata {
    pub name: String,
    pub namespace: String,
    #[serde(default)]
    pub generation: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Spec {
    pub image: String,
    pub resources: Resources,
    pub config_from: String,
    #[serde(default)]
    pub bootstrap_from: Option<String>,
    #[serde(default)]
    pub secrets: HashMap<String, String>,
    pub runtime: Runtime,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Resources {
    pub cpu: String,
    pub memory: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Runtime {
    Ecs(EcsRuntime),
    Kubernetes(KubernetesRuntime),
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EcsRuntime {
    #[serde(default = "default_capacity_provider")]
    pub capacity_provider: String,
    pub networking: EcsNetworking,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EcsNetworking {
    pub subnets: Vec<String>,
    pub security_groups: Vec<String>,
    #[serde(default)]
    pub assign_public_ip: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct KubernetesRuntime {
    #[serde(default)]
    pub node_selector: HashMap<String, String>,
    #[serde(default)]
    pub service_account: Option<String>,
    #[serde(default)]
    pub tolerations: Vec<serde_yaml::Value>,
}

fn default_capacity_provider() -> String {
    "FARGATE".to_string()
}

/// Valid ECS Fargate CPU/memory combinations
const VALID_ECS_CPU: &[&str] = &["256", "512", "1024", "2048", "4096"];

impl OABServiceManifest {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.api_version != "oab.dev/v2" {
            anyhow::bail!("unsupported apiVersion: {} (expected oab.dev/v2)", self.api_version);
        }
        if self.kind != "OABService" {
            anyhow::bail!("unsupported kind: {}", self.kind);
        }
        if self.metadata.name.is_empty() {
            anyhow::bail!("metadata.name is required");
        }
        if self.metadata.namespace.is_empty() {
            anyhow::bail!("metadata.namespace is required");
        }
        if self.spec.image.is_empty() {
            anyhow::bail!("spec.image is required");
        }
        if self.spec.config_from.is_empty() {
            anyhow::bail!("spec.configFrom is required");
        }
        match &self.spec.runtime {
            Runtime::Ecs(ecs) => {
                let valid_cp = ["FARGATE", "FARGATE_SPOT"];
                if !valid_cp.contains(&ecs.capacity_provider.as_str()) {
                    anyhow::bail!("runtime.capacityProvider must be FARGATE or FARGATE_SPOT");
                }
                if ecs.networking.subnets.is_empty() {
                    anyhow::bail!("runtime.networking.subnets must not be empty");
                }
                if ecs.networking.security_groups.is_empty() {
                    anyhow::bail!("runtime.networking.securityGroups must not be empty");
                }
                if !VALID_ECS_CPU.contains(&self.spec.resources.cpu.as_str()) {
                    anyhow::bail!(
                        "spec.resources.cpu must be one of {:?} for ECS runtime",
                        VALID_ECS_CPU
                    );
                }
            }
            Runtime::Kubernetes(_) => {
                // K8S: cpu/memory format validated at deploy time by K8S API
            }
        }
        Ok(())
    }

    pub fn ecs_service_name(&self) -> String {
        format!("oab-{}-{}", self.metadata.namespace, self.metadata.name)
    }
}
