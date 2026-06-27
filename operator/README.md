# oabctl — OAB Agent Provisioner

CLI tool that provisions and manages OpenAB agents on Amazon ECS Fargate (with Kubernetes support planned).

## Quick Start

```bash
# Build
cd operator && cargo build --release

# Deploy an agent
oabctl apply -f examples/kiro-01.yaml

# List running agents
oabctl get oabservice

# Shell into an agent
oabctl exec kiro-01 -- bash

# Copy files
oabctl cp model.bin kiro-01:/data/
oabctl cp kiro-01:/logs/out.log ./

# Sync directories (bidirectional)
oabctl sync ./config kiro-01:/app/config/
oabctl sync kiro-01:/data/ ./backup/

# Delete an agent
oabctl delete oabservice kiro-01 --cluster default --namespace prod
```

## Manifest Schema

```yaml
apiVersion: oab.dev/v2
kind: OABService
metadata:
  name: kiro-01
  namespace: prod
spec:
  image: <ecr-image-uri>
  resources:
    cpu: "256"          # vCPU units
    memory: "512"       # MB
  configFrom: s3://...  # agent config.toml (external)
  bootstrapFrom: s3://... # agent HOME archive (memory, state)
  secrets:
    DISCORD_TOKEN: /oab/prod/kiro-01/discord-token
  runtime:
    type: ecs           # or: kubernetes
    capacityProvider: FARGATE_SPOT
    networking:
      subnets: [subnet-xxx]
      securityGroups: [sg-xxx]
```

### Design Principles

- **Manifest = infra desired state** — what image, how much CPU, where to run
- **Agent config is external** — `configFrom` points to a `config.toml` managed separately
- **Runtime-agnostic spec** — same `image`, `resources`, `secrets` regardless of ECS or K8S
- **Runtime-specific block** — networking, capacity provider, node selectors live under `runtime`

### Kubernetes Runtime (planned)

```yaml
  runtime:
    type: kubernetes
    nodeSelector:
      workload: agents
    serviceAccount: oab-agent
```

### Fleet Manifest

Deploy multiple agents from a single file using `OABFleet`:

```yaml
apiVersion: oab.dev/v2
kind: OABFleet
metadata:
  name: law-shi-team
  namespace: prod
spec:
  template:
    image: <ecr-image-uri>
    resources: { cpu: "256", memory: "512" }
    runtime:
      type: ecs
      capacityProvider: FARGATE_SPOT
      networking: { subnets: [...], securityGroups: [...] }
  agents:
    - name: chaodu
      configFrom: s3://.../chaodu/config.toml
    - name: pudu
      configFrom: s3://.../pudu/config.toml
    - name: openclaw
      configFrom: s3://.../openclaw/config.toml
      image: <different-image>        # override template
      resources: { cpu: "1024", memory: "2048" }  # override
```

Each agent inherits from `template` and can override: `image`, `resources`, `bootstrapFrom`, `secrets`.

**Notes:**
- `runtime` is shared across all agents in a fleet (not overridable per-agent)
- `${name}` is interpolated in `configFrom`, `bootstrapFrom`, and secret values (replaced with agent name)

## JSON Schema

The manifest schema is defined in [`schema/oabservice-v2.json`](schema/oabservice-v2.json) for IDE validation.

## State Store

`oabctl` uses an S3 bucket as its control plane state store (similar to Terraform's S3 backend):

```
s3://<bucket>/
  manifests/{namespace}/{name}.yaml   ← desired state (generation tracked)
```

- **Default bucket:** `oab-control-plane`
- **Override:** set `OAB_CONTROL_PLANE_BUCKET` env var
- Each `oabctl apply` increments the `generation` counter in the stored manifest
- This enables drift detection and rollback tracking in future versions

## Commands

| Command | Description |
|---------|-------------|
| `oabctl apply -f <file\|dir>` | Create or update agents from manifests |
| `oabctl get oabservice [name]` | List agents and their ECS status |
| `oabctl delete oabservice <name>` | Teardown agent (ECS + S3 cleanup) |
| `oabctl exec <agent> -- <cmd>` | Execute command in agent container |
| `oabctl cp <src> <dst>` | Copy files to/from agent containers |
| `oabctl sync <src> <dst>` | Sync directories (bidirectional) |

## Prerequisites

1. **AWS credentials** — IAM role/profile with ECS, SSM, S3 permissions
2. **S3 bucket** — `oab-control-plane` (or set `OAB_CONTROL_PLANE_BUCKET` env var)
3. **ECS cluster** — default cluster or specify with `--cluster`
4. **VPC** — subnets + security groups for Fargate tasks
5. **ECR image** — OAB container image pushed to ECR
6. **SSM parameters** — bot tokens stored at paths referenced in `secrets`
7. **Container requirements** — `curl` or `wget` (+ `tar` for sync)

## How It Works

1. `oabctl apply` validates the manifest, uploads to S3, registers an ECS task definition, and creates/updates the ECS service.
2. ECS maintains the desired state — restarts failed tasks, handles rolling deployments.
3. On task startup, `entrypoint.sh` downloads the bootstrap archive and config from S3, then starts OpenAB.
4. `exec`/`cp`/`sync` use the `ecsctl` library for container operations via ECS Exec + S3 presigned URLs.
