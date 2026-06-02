# Output Directives

## Overview

Agents can control platform-specific message delivery by prefixing their output with `[[key:value]]` directives. OAB parses and strips these before sending to the platform.

## Format

```
[[reply_to:1502606076451885136]]
[[attach_image:/home/node/.codex/generated_images/out.png]]
[[ephemeral:true]]              ← future
Actual message content starts here...
```

Rules:
- Consecutive `[[key:value]]` lines at the start of output = directive header block
- First line that doesn't match `[[key:value]]` (with colon) = content begins
- `attach_image` may also appear later as a standalone directive line outside code blocks; this supports generated-image workflows where the agent writes explanatory text before the path is known
- `[[X]]` without colon is NOT a directive — stops parsing, preserved as content
- Directives are stripped from the final message (never visible to users)
- Unknown keys are silently ignored (forward compatible, logged at debug level)
- If the same key appears multiple times, the last value wins

## Available Directives

### `reply_to`

Reply to a specific message by ID (Discord: `message_reference`).

```
[[reply_to:1502606076451885136]]
Here is my reply to that specific message.
```

**Value**: Platform message ID. Format depends on the target adapter — Discord requires a numeric snowflake; Slack accepts `ts` (e.g. `1234567890.123456`). The directive parser validates that the value is non-empty, ≤64 chars, and contains only ASCII alphanumeric characters plus `.`, `-`, `_`; per-platform format validation happens in each adapter.

**Behavior**:
- Discord: sends with `message_reference`, showing the native "replying to..." UI
- Feishu: sends via Reply API (`POST /im/v1/messages/{id}/reply`), showing native quote UI
- Invalid/non-existent message ID: silently falls back to plain send
- Works in both streaming and send-once modes

**How agents get message IDs**: Every incoming message includes `message_id` in `SenderContext`:

```json
{
  "schema": "openab.sender.v1",
  "sender_id": "845835116920307722",
  "sender_name": "pahud.hsieh",
  "message_id": "1502606076451885136",
  "channel": "discord",
  ...
}
```

## Multi-Agent Use Case

In a thread with multiple bots, agents can reply to each other's messages:

```
Human: "Review this PR" (message_id: 100)
Bot A: "Found 3 issues" (message_id: 101)
Bot B output:
  [[reply_to:101]]
  I agree with Bot A on F1, but F2 is actually fine because...
```

This creates clear visual conversation threads within a Discord thread — essential for multi-agent collaboration.

## Comparison with Other Platforms

| Platform | Reply Mechanism | Agent Control |
|----------|----------------|---------------|
| OpenClaw | `replyToMode` config (`off`/`first`/`all`) | ❌ Platform decides, always to trigger msg |
| Hermes Agent | `DISCORD_REPLY_TO_MODE` env var | ❌ Platform decides, always to trigger msg |
| **OAB** | `[[reply_to:message_id]]` directive | ✅ Agent chooses any message |

> **Note:** `reply_to` is currently implemented for Discord and Feishu (gateway). Slack message IDs (ts format like `1234567890.123456`) are accepted by the parser but the Slack adapter does not yet send threaded replies via this directive — it falls back to plain send. Slack support can be added in a future PR.

### `attach_image`

Attach a local image file produced by the agent to the outgoing user-facing response.

```
[[attach_image:/home/node/.codex/generated_images/sky.png]]
Here is the generated image.
```

**Value**: Local file path visible to the OpenAB process. Relative paths are resolved from `[agent].working_dir`.

**Configuration**: Disabled by default. Enable with:

```toml
[attachments]
enabled = true
```

When `attachments.allowed_dirs` is empty, only `[agent].working_dir` is allowed. Set `allowed_dirs` to permit additional output directories such as Codex's generated image folder:

```toml
[attachments]
enabled = true
allowed_dirs = ["/home/node", "/home/node/.codex/generated_images"]
```

**Behavior**:
- Supported output platforms: Discord.
- Supported formats: PNG, JPEG, GIF, WebP.
- Files are read by OpenAB, validated as images, capped by `attachments.max_bytes`, then uploaded as Discord message attachments.
- Multiple `attach_image` directives are allowed; `attachments.max_files` caps files per response.
- Unlike other directives, `attach_image` can be emitted either in the initial directive header or as a standalone directive line later in the response.
- Directives are stripped from visible text.
- If validation or upload fails, OpenAB sends a warning instead of exposing the directive.

**Security**: OpenAB canonicalizes the requested file path and only reads files under `attachments.allowed_dirs` (or `[agent].working_dir` by default). Do not set `allowed_dirs` wider than the directory where agents intentionally write shareable artifacts.
