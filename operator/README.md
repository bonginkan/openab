# oabctl — OAB Agent Provisioner

CLI tool that provisions and manages OpenAB agents on Amazon ECS Fargate (with Kubernetes support planned).

## How It Works

```
┌─────────────────────────────────────────────────────────────────────────┐
│  Developer Machine                                                       │
│                                                                          │
│  oabctl bootstrap ──► Creates: ECS Cluster, IAM Roles, S3, SG, Logs    │
│                                                                          │
│  oabctl create ─────► Wizard → config.toml + manifest.yaml (local)      │
│       │                  │                                               │
│       │                  └─► Secrets Manager: oab/{ns}/{name}            │
│       │                                                                  │
│  oabctl apply                                                            │
│       │                                                                  │
│       ├─► S3: Upload config.toml to artifacts/{ns}/{name}/              │
│       ├─► ECS: Register Task Definition                                  │
│       └─► ECS: Create/Update Service                                     │
│                                                                          │
│  oabctl exec/cp/sync ──► ecsctl library ──► ECS Exec (SSM)             │
└──────────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────────┐
│  AWS Cloud                                                               │
│                                                                          │
│  ┌─────────────┐     ┌──────────────────────────────────────────┐       │
│  │ S3 Bucket   │     │ ECS Cluster (oab)                        │       │
│  │             │     │                                          │       │
│  │ bootstrap/  │     │  ┌─────────────────────────────────┐    │       │
│  │   state.json│     │  │ Fargate Task (agent)             │    │       │
│  │             │     │  │                                  │    │       │
│  │ manifests/  │     │  │  ┌────────────────────────────┐ │    │       │
│  │   *.yaml    │     │  │  │ OpenAB Container           │ │    │       │
│  │             │     │  │  │                            │ │    │       │
│  │ artifacts/  │◄────┼──┼──│ 1. Download config.toml    │ │    │       │
│  │   config.toml     │  │  │ 2. Resolve [secrets.refs]  │─┼────┼──►SM  │
│  │             │     │  │  │ 3. Start agent             │ │    │       │
│  └─────────────┘     │  │  └────────────────────────────┘ │    │       │
│                       │  └─────────────────────────────────┘    │       │
│  ┌──────────────┐    └──────────────────────────────────────────┘       │
│  │ Secrets Mgr  │                                                        │
│  │ oab/{ns}/{n} │    ┌───────────────┐                                  │
│  │  BOT_TOKEN   │    │ CloudWatch    │                                  │
│  │  STT_API_KEY │    │ /oab/agents   │                                  │
│  └──────────────┘    └───────────────┘                                  │
└─────────────────────────────────────────────────────────────────────────┘
```

## Quick Start

```bash
# 1. Bootstrap infrastructure (one-time)
oabctl bootstrap

# 2. Create an agent (generates config + manifest)
oabctl create my-bot

# 3. Review generated files, then deploy
oabctl apply -f my-bot/manifest.yaml --wait

# 4. Done! Agent is running.
oabctl exec my-bot -- bash
```

Or skip the review step:
```bash
oabctl create my-bot --auto-apply   # generate + deploy in one shot
```

## Complete Workflow

### Step 1: Bootstrap (one-time)

```bash
oabctl bootstrap
```

Creates all required AWS infrastructure with one command. Shows a plan and asks for confirmation before creating anything.

### Step 2: Create Agent (wizard)

```bash
oabctl create my-bot
```

Interactive wizard that:
1. Selects backend platform (kiro/claude-code/codex/gemini/copilot/opencode)
2. Selects release channel (stable/beta) → resolves official image URI
3. Prompts for Discord bot token (masked input) → stores in Secrets Manager
4. Prompts for STT API key (optional, masked) → stores in same secret
5. Selects runtime (ECS)
6. Selects capacity provider (FARGATE_SPOT/FARGATE)
7. Selects VPC
8. Auto-selects subnets (private+NAT priority, 2-3 AZ)
9. Selects or creates security group
10. Generates local files → confirms → applies

Output:
```
my-bot/
├── config.toml      ← OpenAB agent configuration
└── manifest.yaml    ← oabctl deployment manifest
```

### Step 3: Day-to-day Operations

```bash
oabctl get oabservice                # list agents
oabctl exec my-bot -- bash           # shell into container
oabctl cp data.bin my-bot:/tmp/      # upload file
oabctl sync ./skills my-bot:/home/agent/.kiro/skills/  # sync directory
```

### Updating Config

```bash
vim my-bot/config.toml                           # edit locally
oabctl apply -f my-bot/manifest.yaml             # syncs config + redeploys
oabctl apply -f my-bot/manifest.yaml --no-sync   # redeploy only (skip config sync)
```

### Fleet Deploy

```bash
oabctl apply -f fleet.yaml           # deploy 10+ agents from one file
```

## Configuration

### Local Config (`~/.oabctl/config.toml`)

Auto-created by `bootstrap`. Stores persistent settings:

```toml
[defaults]
namespace = "prod"
cluster = "oab"
# region = "us-east-1"

[bootstrap]
bucket = "oab-control-plane-123456789"
```

**Priority:** config.toml > `OAB_CONTROL_PLANE_BUCKET` env var > auto-derive from account

### Agent Config (`{name}/config.toml`)

Generated by `oabctl create`, uploaded to S3 via `apply --sync`. This is the OpenAB runtime configuration:

```toml
[secrets.refs]
discord_bot_token = "aws-sm://oab/prod/my-bot#DISCORD_BOT_TOKEN"
stt_api_key = "aws-sm://oab/prod/my-bot#STT_API_KEY"

[discord]
bot_token = "${secrets.discord_bot_token}"
allow_all_channels = true
allow_all_users = true
max_bot_turns = 1000
message_processing_mode = "per-thread"

[agent]
inherit_env = ["AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "AWS_DEFAULT_REGION"]

[pool]
max_sessions = 5
session_ttl_hours = 1

[reactions]
enabled = true

[stt]
enabled = true
api_key = "${secrets.stt_api_key}"
model = "whisper-large-v3-turbo"
base_url = "https://api.groq.com/openai/v1"

[cron]
usercron_enabled = true
usercron_path = "cronjob.toml"
```

Secrets are resolved by OpenAB at runtime using the task role's Secrets Manager permissions.

## Manifest Schema (oab.dev/v2)

### OABService — single agent

```yaml
apiVersion: oab.dev/v2
kind: OABService
metadata:
  name: my-bot
  namespace: prod
spec:
  image: public.ecr.aws/oablab/kiro:stable
  resources:
    cpu: "256"
    memory: "512"
  configFrom: s3://oab-control-plane-123456789/artifacts/prod/my-bot/config.toml
  runtime:
    type: ecs
    capacityProvider: FARGATE_SPOT
    networking:
      subnets: [subnet-aaa, subnet-bbb]
      securityGroups: [sg-xxx]
```

### OABFleet — batch deploy

```yaml
apiVersion: oab.dev/v2
kind: OABFleet
metadata:
  name: my-team
  namespace: prod
spec:
  template:
    image: public.ecr.aws/oablab/kiro:stable
    resources: { cpu: "256", memory: "512" }
    runtime:
      type: ecs
      capacityProvider: FARGATE_SPOT
      networking: { subnets: [...], securityGroups: [...] }
  agents:
    - name: bot-a
      configFrom: s3://.../bot-a/config.toml
    - name: bot-b
      configFrom: s3://.../bot-b/config.toml
      resources: { cpu: "1024", memory: "2048" }  # override
```

**Fleet features:**
- Template inheritance with per-agent overrides (`image`, `resources`, `bootstrapFrom`, `secrets`)
- `${name}` interpolation in `configFrom`, `bootstrapFrom`
- Runtime shared across fleet (not overridable per-agent)
- Validate-all-before-apply (no partial deploys)

### Design Principles

- **Manifest = infra desired state** — image, CPU, networking
- **Agent config is external** — `configFrom` points to config.toml (managed via `--sync`)
- **Secrets resolved by OpenAB** — `[secrets.refs]` in config.toml, not in manifest
- **Runtime-agnostic spec** — same top-level fields regardless of ECS or K8S

## Bootstrap

One-time infrastructure setup — similar to `cdk bootstrap`:

```bash
oabctl bootstrap                          # create all (with plan + Y/n)
oabctl bootstrap --status                 # show current state
oabctl bootstrap --delete                 # teardown (only managed resources)
oabctl bootstrap --cluster my-cluster     # import existing resources
```

### Resources Created

| Resource | Name | Purpose |
|----------|------|---------|
| ECS Cluster | `oab` | FARGATE + FARGATE_SPOT |
| IAM Role | `oab-task-execution` | Pull images from ECR |
| IAM Role | `oab-task-role` | ECS Exec + S3 artifacts + Secrets Manager |
| S3 Bucket | `oab-control-plane-{account}` | State + artifacts |
| Security Group | `oab-agents` | Outbound-only |
| Log Group | `/oab/agents` | CloudWatch logs |

### IAM Task Role Permissions

| Policy | Permissions | Resource |
|--------|------------|----------|
| `oab-ecs-exec` | ssmmessages:* | * (ECS Exec requirement) |
| `oab-s3-artifacts` | s3:GetObject, s3:PutObject | `{bucket}/artifacts/*` |
| `oab-secrets` | secretsmanager:GetSecretValue | `arn:aws:secretsmanager:*:*:secret:oab/*` |

### Import Existing Resources

```bash
oabctl bootstrap \
  --cluster my-existing-cluster \
  --vpc vpc-12345 \
  --subnets subnet-a,subnet-b \
  --security-group sg-existing \
  --execution-role arn:aws:iam::123:role/my-role \
  --task-role arn:aws:iam::123:role/my-task-role
```

Imported resources are tracked but not deleted on `bootstrap --delete`.

## State Store

```
s3://oab-control-plane-{account}/
├── bootstrap/state.json              ← infra state (resource ARNs, managed flags)
├── manifests/{namespace}/{name}.yaml ← desired state (generation tracked)
└── artifacts/{namespace}/{name}/     ← agent configs, accessible by task role
    └── config.toml
```

## Commands

| Command | Description |
|---------|-------------|
| `oabctl bootstrap` | One-time infra setup (plan + confirm) |
| `oabctl bootstrap --delete` | Teardown managed resources |
| `oabctl bootstrap --status` | Show bootstrap state |
| `oabctl create <name>` | Interactive wizard → generate config + manifest |
| `oabctl create <name> --auto-apply` | Generate + deploy immediately |
| `oabctl apply -f <file\|dir>` | Sync config + deploy (default) |
| `oabctl apply -f <file> --no-sync` | Deploy without syncing config |
| `oabctl get oabservice [name]` | List agents and status |
| `oabctl delete oabservice <name>` | Teardown agent |
| `oabctl exec <agent> -- <cmd>` | Execute command in container |
| `oabctl cp <src> <dst>` | Copy files to/from container |
| `oabctl sync <src> <dst>` | Sync directories (bidirectional) |

## JSON Schema

[`schema/oabservice-v2.json`](schema/oabservice-v2.json) — supports both OABService and OABFleet for IDE validation.

## Prerequisites

With `oabctl bootstrap`, most prerequisites are handled automatically. You only need:

1. **AWS credentials** — IAM user/role with permissions to create the above resources
2. **Docker** — to build custom images (optional if using official images)

