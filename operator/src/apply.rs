use crate::manifest::{OABFleetManifest, OABServiceManifest, RawManifest, Runtime};
use anyhow::{Context, Result};
use aws_sdk_ecs::types::{
    AssignPublicIp, AwsVpcConfiguration, CapacityProviderStrategyItem, ContainerDefinition,
    KeyValuePair, NetworkConfiguration, Secret,
};
use aws_sdk_s3::primitives::ByteStream;
use std::path::Path;

pub async fn run(aws_config: &aws_config::SdkConfig, file_path: &str) -> Result<()> {
    let path = Path::new(file_path);
    let manifests = load_manifests(path)?;

    if manifests.is_empty() {
        anyhow::bail!("no manifests found at {}", file_path);
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
        apply_ecs(&ecs, &s3, m).await?;
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
    m: &OABServiceManifest,
) -> Result<()> {
    let ecs_rt = match &m.spec.runtime {
        Runtime::Ecs(rt) => rt,
        _ => unreachable!(),
    };

    let service_name = m.ecs_service_name();
    let bucket = std::env::var("OAB_CONTROL_PLANE_BUCKET")
        .unwrap_or_else(|_| "oab-control-plane".to_string());

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
        .cluster("default")
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
            .cluster("default")
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
            .cluster("default")
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

    Ok(())
}
