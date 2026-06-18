# Sending Images Back to Discord

> **This doc is designed for your coding agent.** Share it with your agent so it learns how to send images back to Discord.
>
> Example prompt:
> ```
> Read docs/sendimages.md from OpenAB GitHub and send the image back to my Discord thread.
> ```
>
> 💡 **Tip:** If it works the first time, ask your agent to save this as a **SKILL** so it remembers how to do it next time without re-reading the doc.

---

OpenAB can relay agent-generated files and images to Discord when outbound
attachments are enabled. This page covers image-specific behavior. The agent
writes an image file, then prefixes its response with an `attach_image` output
directive. OpenAB validates the file as an image and uploads it through the
Discord bot.

> For sending non-image files (PDF, CSV, logs, etc.), see [sendfiles.md](sendfiles.md).

## How It Works

```
┌──────────┐  message + file   ┌──────────┐  ACP stdio   ┌──────────────┐
│ Discord  │◄──────────────────│ OpenAB   │◄────────────│ Agent (CLI)  │
│ Thread   │                   └────┬─────┘             └──────┬───────┘
│          │                        │                          │
│          │ Discord REST API       │ read/validate file        │ writes image
│          │ POST /messages         │ from allowed dir          │ emits directive
│          │ + files[n]             │                          │
└──────────┘                        └──────────────────────────┘
```

OpenAB streams text over ACP, but it also parses output directives from the
agent response. `[[attach_image:path]]` is stripped from visible text and used
only as delivery metadata; it can be placed in the initial directive header or
later in the response outside code blocks.

## Step-by-Step

### 1. Enable outbound attachments

Outbound attachments are off by default.

```toml
[attachments]
enabled = true
```

When `allowed_dirs` is omitted or empty, OpenAB only allows files under
`[agent].working_dir`. For Codex image generation, include Codex's generated
image directory:

```toml
[agent]
working_dir = "/home/node"

[attachments]
enabled = true
allowed_dirs = ["/home/node", "/home/node/.codex/generated_images"]
max_bytes = 10485760
max_files = 10
```

If agents sometimes emit image paths from temporary staging folders, keep
`allowed_dirs` restricted and opt in to auto-staging instead of widening the
allowlist:

```toml
[attachments]
enabled = true
allowed_dirs = ["/home/node"]
auto_stage_generated_images = true
auto_stage_dir = "/home/node/out"
```

### 2. Have the agent emit `attach_image`

```
[[attach_image:/home/node/.codex/generated_images/sky.png]]
Here is the generated image.
```

Relative paths are resolved from `[agent].working_dir`.

### 3. Permissions

OpenAB uses the existing Discord bot token from `[discord] bot_token`; the token
is not forwarded to the agent subprocess.

The bot needs:

- `Send Messages`
- `Send Messages in Threads`
- `Attach Files`

## Legacy Direct Upload Pattern

Older deployments can still use an out-of-band uploader or sidecar that calls the
Discord Create Message endpoint directly with `multipart/form-data`. Native
OpenAB relay is preferred because the agent does not need a Discord token.

## Automated Sidecar Pattern

If your agent generates images to a known directory (e.g. Codex writes to
`~/.codex/generated_images/`), you can run a **file-watcher sidecar** that
automatically uploads new images:

1. Watch the output directory for new files.
2. Read the session metadata to find the originating `thread_id`.
3. Upload via the Discord API.
4. Track uploaded files in a state file to avoid duplicates.

This is the pattern used by the community `discord-image-uploader` sidecar.

## Security Considerations

- **Do not expose Discord tokens to the agent.** Native relay uses OpenAB's existing bot token.
- **Scope permissions.** The bot only needs `Send Messages`, `Send Messages in Threads`, and `Attach Files` in the target channels.
- **Restrict `attachments.allowed_dirs`.** Only include directories intended for shareable generated artifacts.
- **Rate limits.** Discord enforces rate limits on message creation. Space uploads if sending multiple images.

## Bot Permission Checklist

In the [Discord Developer Portal](https://discord.com/developers/applications), ensure your bot has:

- [x] `Send Messages`
- [x] `Send Messages in Threads`
- [x] `Attach Files`

These are typically already granted if your bot works with OpenAB.

## FAQ

**Q: Can OpenAB relay images natively?**
A: Yes, when `[attachments].enabled = true` and the agent emits `[[attach_image:path]]`.

**Q: Does this work with Slack / Telegram / LINE?**
A: Native outbound attachment relay is currently implemented for Discord. Other platforms still need a platform-specific uploader or future adapter support.

**Q: Can OpenAB relay non-image files?**
A: Yes. Use `[[attach_file:path]]`; see [sendfiles.md](sendfiles.md).

**Q: What image formats are supported?**
A: PNG, JPEG, GIF, and WebP. The OpenAB default per-file cap is 10 MiB, matching Discord's current default app upload limit.
