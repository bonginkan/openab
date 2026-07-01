use crate::bootstrap::BootstrapState;
use crate::manifest::{OABFleetManifest, OABServiceManifest, RawManifest, Runtime};
use anyhow::{Context, Result};
use aws_sdk_ecs::types::{
    AssignPublicIp, AwsVpcConfiguration, CapacityProviderStrategyItem, ContainerDefinition,
    KeyValuePair, NetworkConfiguration, Secret,
};
use aws_sdk_s3::primitives::ByteStream;
use std::path::Path;

/// Try to load bootstrap state for networking defaults (used in future for auto-fill)
#[allow(dead_code)]
async fn load_bootstrap_state(config: &aws_config::SdkConfig) -> Option<BootstrapState> {
    let sts = aws_sdk_sts::Client::new(config);
    let account = sts.get_caller_identity().send().await.ok()?
        .account()?.to_string();
    let bucket = format!("oab-control-plane-{account}");
    let s3 = aws_sdk_s3::Client::new(config);
    crate::bootstrap::load_state_pub(&s3, &bucket).await.ok().flatten()
}

pub async fn run(aws_config: &aws_config::SdkConfig, file_path: &str, sync_config: bool, wait: bool) -> Result<()> {
    let path = Path::new(file_path);
    let manifests = load_manifests(path)?;

    if manifests.is_empty() {
        anyhow::bail!("no manifests found at {}", file_path);
    }

    // --sync: upload local config.toml to S3 configFrom path
    if sync_config {
        let s3 = aws_sdk_s3::Client::new(aws_config);
        for m in &manifests {
            let config_path = path.parent().unwrap_or(Path::new(".")).join("config.toml");
            if config_path.exists() && !m.spec.config_from.is_empty() {
                let body = aws_sdk_s3::primitives::ByteStream::from_path(&config_path).await
                    .context("failed to read local config.toml")?;
                // Parse s3://bucket/key from configFrom
                if let Some(s3_path) = m.spec.config_from.strip_prefix("s3://") {
                    let (bucket, key) = s3_path.split_once('/').context("invalid configFrom S3 URI")?;
                    s3.put_object().bucket(bucket).key(key).body(body).send().await
                        .context("failed to sync config.toml to S3")?;
                    eprintln!("  ⬆ Synced config.toml → {}", m.spec.config_from);
                }
            }
        }
    }

    let ecs = aws_sdk_ecs::Client::new(aws_config);
    let s3 = aws_sdk_s3::Client::new(aws_config);

    // Validate ALL manifests before applying any (prevent partial apply)
    for m in &manifests {
        m.validate()?;
        if matches!(&m.spec.runtime, Runtime::Kubernetes(_)) {
            anyhow::bail!(
                "Kubernetes runtime not yet implemented (manifest: {})",
                m.metadata.name
            );
        }
    }

    for m in &manifests {
        println!("  Applying {} (ECS)...", m.metadata.name);
        apply_ecs(&ecs, &s3, aws_config, m, wait).await?;
    }

    println!("\n{} service(s) applied.", manifests.len());
    Ok(())
}

fn load_manifests(path: &Path) -> Result<Vec<OABServiceManifest>> {
    let mut manifests = Vec::new();
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "yaml" || e == "yml") {
                manifests.extend(parse_manifest_file(&p)?);
            }
        }
    } else {
        manifests.extend(parse_manifest_file(path)?);
    }
    Ok(manifests)
}

/// Parse a YAML file — returns one or more OABServiceManifests (fleet expands to many)
fn parse_manifest_file(path: &Path) -> Result<Vec<OABServiceManifest>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    // Detect kind first
    let raw: RawManifest = serde_yaml::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    match raw.kind.as_str() {
        "OABService" => {
            let m: OABServiceManifest = serde_yaml::from_str(&content)
                .with_context(|| format!("failed to parse OABService {}", path.display()))?;
            Ok(vec![m])
        }
        "OABFleet" => {
            let fleet: OABFleetManifest = serde_yaml::from_str(&content)
                .with_context(|| format!("failed to parse OABFleet {}", path.display()))?;
            fleet.validate()?;
            println!("  Fleet '{}': expanding {} agents...", fleet.metadata.name, fleet.spec.agents.len());
            Ok(fleet.expand())
        }
        other => anyhow::bail!("unsupported kind '{}' in {}", other, path.display()),
    }
}

async fn apply_ecs(
    ecs: &aws_sdk_ecs::Client,
    s3: &aws_sdk_s3::Client,
    config: &aws_config::SdkConfig,
    m: &OABServiceManifest,
    wait: bool,
) -> Result<()> {
    let ecs_rt = match &m.spec.runtime {
        Runtime::Ecs(rt) => rt,
        _ => unreachable!(),
    };

    let service_name = m.ecs_service_name();
    let bucket = if let Some(b) = crate::config::OabConfig::load().ok().and_then(|c| c.bucket()) {
        b
    } else {
        // Fallback: derive from account ID
        let sts = aws_sdk_sts::Client::new(config);
        let account = sts.get_caller_identity().send().await
            .ok().and_then(|r| r.account().map(|a| a.to_string()))
            .unwrap_or_else(|| "unknown".to_string());
        format!("oab-control-plane-{account}")
    };

    // Read current generation from S3 manifest (if exists), increment
    let manifest_key = format!("manifests/{}/{}.yaml", m.metadata.namespace, m.metadata.name);
    let current_gen = match s3.get_object().bucket(&bucket).key(&manifest_key).send().await {
        Ok(resp) => {
            let bytes = resp.body.collect().await?.into_bytes();
            let existing: OABServiceManifest = serde_yaml::from_slice(&bytes)?;
            existing.metadata.generation
        }
        Err(_) => 0,
    };
    let generation = current_gen + 1;

    // 1. Upload manifest to S3 (record of desired state)
    let mut manifest_to_store = serde_yaml::to_value(m)?;
    manifest_to_store["metadata"]["generation"] = serde_yaml::Value::Number(generation.into());
    let manifest_yaml = serde_yaml::to_string(&manifest_to_store)?;
    s3.put_object()
        .bucket(&bucket)
        .key(&manifest_key)
        .body(ByteStream::from(manifest_yaml.into_bytes()))
        .send()
        .await
        .context("failed to upload manifest to S3")?;

    // 2. Build environment variables
    let mut env_vars = vec![
        KeyValuePair::builder().name("NAMESPACE").value(&m.metadata.namespace).build(),
        KeyValuePair::builder().name("NAME").value(&m.metadata.name).build(),
    ];
    if !m.spec.config_from.is_empty() {
        env_vars.push(KeyValuePair::builder().name("CONFIG_S3_PATH").value(&m.spec.config_from).build());
    }
    if let Some(ref bootstrap) = m.spec.bootstrap_from {
        env_vars.push(KeyValuePair::builder().name("BOOTSTRAP_FROM").value(bootstrap).build());
    }

    // 3. Build secrets from map
    let secrets: Vec<Secret> = m
        .spec
        .secrets
        .iter()
        .map(|(name, ssm_path)| {
            Secret::builder().name(name).value_from(ssm_path).build().unwrap()
        })
        .collect();

    // 4. Register task definition
    let container = ContainerDefinition::builder()
        .name("openab")
        .image(&m.spec.image)
        .essential(true)
        .set_environment(Some(env_vars))
        .set_secrets(if secrets.is_empty() { None } else { Some(secrets) })
        .build();

    let task_def = ecs
        .register_task_definition()
        .family(&service_name)
        .requires_compatibilities(aws_sdk_ecs::types::Compatibility::Fargate)
        .network_mode(aws_sdk_ecs::types::NetworkMode::Awsvpc)
        .cpu(&m.spec.resources.cpu)
        .memory(&m.spec.resources.memory)
        .container_definitions(container)
        .send()
        .await
        .context("failed to register task definition")?;

    let task_def_arn = task_def
        .task_definition()
        .and_then(|td| td.task_definition_arn())
        .unwrap_or_default()
        .to_string();

    // 5. Create or update ECS service
    let assign_ip = if ecs_rt.networking.assign_public_ip {
        AssignPublicIp::Enabled
    } else {
        AssignPublicIp::Disabled
    };

    let vpc_config = AwsVpcConfiguration::builder()
        .set_subnets(Some(ecs_rt.networking.subnets.clone()))
        .set_security_groups(Some(ecs_rt.networking.security_groups.clone()))
        .assign_public_ip(assign_ip)
        .build()?;

    let network_config = NetworkConfiguration::builder()
        .awsvpc_configuration(vpc_config)
        .build();

    // Check if service exists
    let existing = ecs
        .describe_services()
        .cluster("oab")
        .services(&service_name)
        .send()
        .await;

    let service_active = existing
        .as_ref()
        .ok()
        .and_then(|r| r.services().first())
        .is_some_and(|s| s.status() == Some("ACTIVE"));

    if service_active {
        ecs.update_service()
            .cluster("oab")
            .service(&service_name)
            .task_definition(&task_def_arn)
            .network_configuration(network_config)
            .send()
            .await
            .context("failed to update ECS service")?;
        println!("  ✓ {} updated", m.metadata.name);
    } else {
        let cap_strategy = CapacityProviderStrategyItem::builder()
            .capacity_provider(&ecs_rt.capacity_provider)
            .weight(1)
            .build()?;

        ecs.create_service()
            .cluster("oab")
            .service_name(&service_name)
            .task_definition(&task_def_arn)
            .desired_count(1)
            .capacity_provider_strategy(cap_strategy)
            .network_configuration(network_config)
            .send()
            .await
            .context("failed to create ECS service")?;
        println!(
            "  ✓ {} created ({}, {}cpu/{}mem)",
            m.metadata.name, ecs_rt.capacity_provider, m.spec.resources.cpu, m.spec.resources.memory
        );
    }

    if wait {
        eprintln!("  ⏳ Waiting for {} to stabilize...", m.metadata.name);
        wait_for_stable(ecs, "oab", &service_name).await?;
        eprintln!("  ✓ {} is stable", m.metadata.name);
    }

    Ok(())
}

async fn wait_for_stable(ecs: &aws_sdk_ecs::Client, cluster: &str, service: &str) -> Result<()> {
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let resp = ecs.describe_services()
            .cluster(cluster)
            .services(service)
            .send().await?;
        if let Some(svc) = resp.services().first() {
            let deployments = svc.deployments();
            if deployments.len() == 1 {
                if let Some(d) = deployments.first() {
                    if d.running_count() == d.desired_count() && d.rollout_state() == Some(&aws_sdk_ecs::types::DeploymentRolloutState::Completed) {
                        return Ok(());
                    }
                }
            }
        }
    }
    anyhow::bail!("timed out waiting for service to stabilize (5 min)")
}
