# agentd X adapter

A Rust X (Twitter) adapter for [agentd](https://github.com/minifish-org/agentd).
Only one configured owner can trigger the bot by explicitly mentioning it.
Designed for `singapore` (AWS Lightsail), calling agentd on `minifish-lab`
through Tailscale. No public inbound port, webhook server or model process is
needed on Lightsail.

```text
X mentions → owner admission → reply ancestors + quoted posts + images
           → tenant-scoped agentd turn → local/chat via tailgate
agentd delivery outbox → X reply → delivery acknowledgment
```

## Behavior

- OAuth 1.0a using API Key/Secret and Access Token/Secret; checks the authenticated
  bot username at startup. The owner's numeric user ID is pinned in configuration.
- Polls mentions every 60 seconds by default. Persists all pages before advancing
  the high-water mark; idempotent submissions survive restarts. A new database
  starts at the current time, avoiding replies to historical mentions.
- Retrieves the root, the complete accessible ancestor chain, and referenced
  quotes, up to 32 posts. This is the question's context, not every sibling reply
  in a viral conversation. Missing/deleted/private posts are explicitly flagged.
- Reads long-post `note_tweet` text. Downloads photos only from `pbs.twimg.com`,
  with redirects disabled, a bounded download and bounded image decoder. Resizes
  at most four photos to at most 768px and 128 KiB each. Videos/GIFs and omitted
  photos are reported as unviewed; it never substitutes alt text for actual sight.
- Dedicated `x-agentd` tenant; agentd's outbox does not filter by transport.
  **Never share this tenant with the Telegram adapter or another outbox consumer.**
- Replies are only sent from claimed outbox rows matched to admitted local jobs.
  Uses one short plain-text reply, no additional mentions or links. The conservative
  140-Unicode-scalar limit guarantees the weighted X 280-character ceiling.
  Oversized answers fail instead of being silently truncated.
- A persisted send receipt prevents reposting after an acknowledgment failure.
  A crash/network error/5xx during posting is quarantined as `unknown`; no blind
  retry. Review the X account manually before recovering such a job. There is no
  claim of exactly-once delivery across X and the local database.
- Rate-limit responses respect retry hints. Only explicit 429 rejections are
  retried after a publish attempt; other ambiguous results are held for review.
- Preview mode is the default: turns have **no delivery destination**, results
  remain local, and later enabling publishing cannot publish old previews.

## Requirements

Rust 1.94; X developer app with Read and write OAuth 1.0a credentials; an agentd
HTTP endpoint; and the companion agentd inline-image change documented below.
Keep the repository's existing AGPL-3.0 license.

The new agentd input contract is:

```json
{
  "text": "optional text and other application metadata",
  "images": [
    { "url": "data:image/jpeg;base64,...", "caption": "Image attached to post 123" }
  ]
}
```

The companion patch in the sibling agentd repository forwards images as real
`image_url` parts and preserves them in context. **An older agentd silently
serializes images as text and cannot see them. Deploy that patch first.**
Then test vision through tailgate's `local/chat`; a model name alone is not proof
that the deployed inference server has a working vision projector.

## Configure and verify

Start from [configs/x-adapter.env.example](configs/x-adapter.env.example).
Keep the populated file outside Git, mode 0600. Source it in the shell or use
systemd's EnvironmentFile; the program does not automatically load `.env` files.
Credentials are never logged; errors omit remote response bodies and headers.

```sh
set -a
. /path/to/private/x-adapter.env
set +a
cargo run --locked -- doctor jackysp
```

Copy the returned numeric owner ID into `X_OWNER_ID`. `doctor` makes authenticated
read-only X calls (billable); it does not publish or inspect secrets.

```sh
cargo run --locked -- register
cargo run --locked -- run
```

`register` creates tenant `x-agentd` and agent `x-bot` only if absent; it preserves
existing agents. The default agent uses `local/chat`, with read-only clock, public-web
search and fetch tools. It has no rolling context (each request contains its X
context), a 30-minute timeout, four model steps and 512 output tokens per step.
The model gateway must support native function calls and tool-result messages.
Memory, artifacts, schedules, MCP and sandbox tools remain disabled.
For the slow local model, deployed agentd uses `http_timeout_secs = 600`
(shared by its HTTP clients), and tailgate local requests allow 660 seconds.
The local-ai execution timeout is 900 seconds. These are upper bounds;
a completed result returns immediately.

Mention `@agentd_ai` from `@jackysp` **after** starting the adapter. In preview mode,
stop the process and inspect:

```sh
cargo run --locked -- status --state /path/to/adapter.db
```

The SQLite database is process-locked. It binds the bot, owner, endpoint, tenant
and agent identities to prevent accidental reuse. It contains private conversation
text and previews; protect it and its backups. Completed local content is removed
after seven days while small ID tombstones remain for deduplication. Pending and
ambiguous jobs are retained for investigation. Agentd run/input retention must be
managed separately; this adapter does not delete agentd data.

## Deploy to Lightsail

1. Ensure Tailscale can reach `https://minifish-lab.taila2cd17.ts.net` from singapore.
2. Install populated configuration as `/etc/agentd-x-adapter.env` (root, mode 0600).
3. Run `doctor` and `register` with this configuration before starting the service.
4. Commit and push `main`, then run `bash deploy/update-vps.sh singapore`.

The deploy script builds with one job on the server, atomically switches the
versioned binary, verifies the service, and rolls back the binary and unit on
startup failure. It never overwrites credentials or registers an agent implicitly.
Requires the Rust toolchain and authenticated Git access for a private repository.
Service activation is not proof of API/vision success; inspect preview output.

```sh
ssh singapore 'sudo journalctl -u agentd-x-adapter -n 80 --no-pager'
```

After obtaining X's written AI-reply approval and verifying previews, set both
`X_AI_REPLY_APPROVED=true` and `X_PUBLISH=true` and restart. Setting `X_PUBLISH=false`
pauses public delivery. OAuth callback URLs are not used by this single-account
OAuth1 client. No credentials are required by the offline tests.

## Verification

```sh
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
bash -n deploy/update-vps.sh
```

Tests cover OAuth signatures, admission, pagination/cursor ordering, original and
quoted context, long posts, photo normalization, outbox-only publishing, preview
isolation, acknowledgment failure and ambiguous delivery/crash recovery.

API references: [mentions](https://docs.x.com/x-api/users/get-mentions),
[conversation IDs](https://docs.x.com/x-api/fundamentals/conversation-id),
[posting](https://docs.x.com/x-api/posts/create-post),
[automation rules](https://help.x.com/en/rules-and-policies/x-automation).
