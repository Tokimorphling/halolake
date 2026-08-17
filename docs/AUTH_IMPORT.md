# Halolake Auth Import

Halolake can turn **third-party credential dumps** into control-plane channels (and proxy pool entries). This is a first-class feature for operators migrating from **Sub2API** and **CLIProxyAPI**, or pasting **Codex / ChatGPT OAuth** material.

> **Implementers / upstream porting:** see [`AUTH_FILE_PORT_CN.md`](./AUTH_FILE_PORT_CN.md) for CLIProxyAPI & Sub2API field maps, runtime headers, and a checklist when `ref/` updates.

## What you can import

| Source | Input | Becomes |
|--------|--------|---------|
| **Sub2API data export** | `{ "type":"sub2api-data", "proxies":[], "accounts":[] }` | Proxy pool rows + channels |
| **CLIProxyAPI auth files** | One or more `*.json` with `"type":"codex"\|"claude"\|"gemini"\|"xai"…` | Channels (Codex=57, Claude=14, Gemini=24, xAI=48) |
| **Codex / Sub2API session** | Nested `tokens.*`, flat OAuth JSON, raw access token / multi-line mix | Codex channels (type 57) |

The importer also accepts Sub2API accounts whose `platform` is `grok` or `xai`. OAuth accounts become xAI/Grok channels with the CLI chat-proxy default; `apikey`, `api_key`, and `upstream` accounts use the official xAI API default. Refresh, team, subscription, and entitlement fields are retained in channel settings.

**Groups are not auto-bound** from Sub2API (same idea as Sub2API’s `skip_default_group_bind`). You can pass a default `group` for new channels; rebind in the UI as needed.

**Proxies** from Sub2API exports are matched by fingerprint (`protocol|host|port|user|pass`); existing proxies are **reused**, not duplicated. Accounts always create **new** channels (unless Codex identity matches and `update_existing` is true).

---

## APIs

Admin role required (same as other channel management APIs). Session cookie / admin auth as usual.

### 1. Unified JSON import (recommended)

```http
POST /api/channel/import/auth
Content-Type: application/json
```

```json
{
  "format": "auto",
  "content": "<single file or paste>",
  "contents": ["<optional multi-file bodies>"],
  "filenames": ["a.json", "b.json"],
  "group": "default",
  "models": "gpt-5.1,gpt-5,o3,o4-mini",
  "proxy_id": null,
  "update_existing": true,
  "name": "optional name prefix",
  "data": null
}
```

| Field | Description |
|-------|-------------|
| `format` | `auto` (default), `cliproxy`, `codex-session`, `sub2api-data` |
| `content` | One blob (file text or paste) |
| `contents` + `filenames` | Batch API upload without multipart |
| `update_existing` | Update identity-matched Codex sessions and CLIProxy Codex/Claude/xAI channels; `false` skips the match |
| `data` | Optional structured Sub2API payload instead of string `content` |

**Response** (business envelope `{ success, data }`):

```json
{
  "success": true,
  "data": {
    "format": "cliproxy",
    "channels": {
      "total": 2,
      "created": 2,
      "updated": 0,
      "skipped": 0,
      "failed": 0,
      "items": [],
      "warnings": [],
      "errors": []
    },
    "data": null,
    "file_results": [
      {
        "name": "user@example.com.json",
        "format": "cliproxy",
        "ok": true,
        "message": "created",
        "channel_id": 3,
        "created": 1
      }
    ]
  }
}
```

When the blob is Sub2API data, `data` is filled with proxy/account counters instead of (or in addition to) channel stats.

### 2. Multipart batch upload (CLIProxyAPI-style)

```http
POST /api/channel/import/auth/upload
Content-Type: multipart/form-data
```

| Part | Description |
|------|-------------|
| `files` / `file` / `auth` | One or more JSON auth files |
| `format` | Optional: `auto` / `cliproxy` / `codex-session` / `sub2api-data` |
| `group`, `models`, `name` | Optional strings |
| `update_existing` | `true` / `false` / `1` / `0` |

```bash
curl -sS -b cookies.txt \
  -F 'files=@codex-user.json' \
  -F 'files=@claude-user.json' \
  -F 'format=cliproxy' \
  -F 'group=default' \
  http://127.0.0.1:9090/api/channel/import/auth/upload
```

### 3. Compatibility endpoints

Still available (same auth):

| Endpoint | Purpose |
|----------|---------|
| `POST /api/channel/import/codex-auth` | Codex / Sub2API session only |
| `POST /api/channel/import/sub2api-data` | Sub2API backup JSON only |

Prefer `/api/channel/import/auth` for new integrations.

---

## Format details

### CLIProxyAPI auth file

Saved under CLIProxyAPI’s `auths/` directory. Minimal shapes Halolake accepts:

**Codex (ChatGPT OAuth) → channel type 57**

```json
{
  "type": "codex",
  "email": "user@example.com",
  "id_token": "...",
  "access_token": "...",
  "refresh_token": "...",
  "account_id": "…",
  "last_refresh": "2026-01-01T00:00:00Z",
  "expired": "1735689600"
}
```

(`account_id` / `expired` / `last_refresh` match CLIProxyAPI’s `CodexTokenStorage`.)

**Claude → channel type 14**

```json
{
  "type": "claude",
  "email": "user@example.com",
  "access_token": "...",
  "refresh_token": "...",
  "expired": "..."
}
```

**Gemini / gemini-cli → channel type 24**

```json
{
  "type": "gemini",
  "api_key": "AIza..."
}
```

or `access_token` if present.

**xAI / Grok OAuth (CLIProxyAPI) → channel type 48**

```json
{
  "type": "xai",
  "auth_kind": "oauth",
  "access_token": "...",
  "refresh_token": "...",
  "id_token": "...",
  "email": "user@example.com",
  "sub": "...",
  "base_url": "https://cli-chat-proxy.grok.com/v1",
  "token_endpoint": "https://auth.x.ai/oauth2/token",
  "headers": {
    "X-XAI-Token-Auth": "xai-grok-cli",
    "x-grok-client-version": "0.2.120",
    "User-Agent": "xai-grok-workspace/0.2.120",
    "x-grok-client-identifier": "grok-shell",
    "x-authenticateresponse": "authenticate-response"
  }
}
```

Notes:
- Runtime key is `access_token` (Bearer). Optional `api_key` is accepted as a fallback field name.
- OAuth defaults to Grok CLI chat-proxy base (`https://cli-chat-proxy.grok.com/v1`); set `using_api: true` or use a non-OAuth/API-key auth kind for the official `https://api.x.ai/v1` endpoint.
- `base_url` may be a provider root or a complete operation URL. The importer removes `/responses`, `/responses/compact`, and similar operation suffixes, preserves xAI/Codex `/v1`, and preserves custom host path prefixes.
- CLIProxy-style OAuth with `using_api: false` rewrites the official `https://api.x.ai/v1` default to the CLI chat-proxy. Set `using_api: true` when the official API is intentional. This differs from Sub2API's runtime, where an explicitly stored OAuth `base_url` is treated as a manual endpoint; include `using_api: true` when importing such a Sub2API account into Halolake.
- The five CLI identity headers shown above are injected automatically for the official CLI chat-proxy. File-provided `headers` are optional and override those defaults case-insensitively.
- `refresh_token` / identity metadata are stored in channel `setting` for later refresh work.
- Re-import with the same email/sub updates the existing xAI channel when `update_existing` is true.

Unsupported CLIProxy types (e.g. antigravity, kimi) return a clear error for that file; other files in a batch still import.

### Sub2API Codex session

Same rules as Sub2API `ImportCodexSession`:

- raw access token (line or whole body)
- `{ "tokens": { "access_token", "refresh_token", "id_token" }, "email", "chatgpt_account_id", ... }`
- arrays / JSON streams / mixed lines

Normalized into the same Codex OAuth key JSON used by channel refresh/usage.

### Sub2API data export

```json
{
  "type": "sub2api-data",
  "version": 1,
  "exported_at": "2026-01-01T00:00:00Z",
  "proxies": [
    {
      "proxy_key": "socks5|1.2.3.4|1080|u|p",
      "name": "us",
      "protocol": "socks5",
      "host": "1.2.3.4",
      "port": 1080,
      "username": "u",
      "password": "p",
      "status": "active"
    }
  ],
  "accounts": [
    {
      "name": "acc",
      "platform": "openai",
      "type": "oauth",
      "credentials": {
        "access_token": "...",
        "refresh_token": "...",
        "chatgpt_account_id": "..."
      },
      "proxy_key": "socks5|1.2.3.4|1080|u|p",
      "concurrency": 3,
      "priority": 50
    }
  ]
}
```

Account mapping:

| platform + type | Halolake channel |
|-----------------|------------------|
| openai + oauth / setup-token | type **57** Codex |
| openai + apikey / upstream | type **1** OpenAI |
| anthropic / claude | type **14** |
| gemini / google | type **24** |
| grok / xai + oauth | type **48** xAI/Grok (CLI chat-proxy default) |
| grok / xai + apikey / upstream | type **48** xAI/Grok (official API default) |

For Sub2API accounts, `credentials.base_url` takes precedence over `extra.base_url`. The value is validated as an absolute `http`/`https` URL and normalized per provider:

| Provider | Stored channel base | Relay operation appended |
|----------|---------------------|--------------------------|
| OpenAI / Anthropic | host and custom prefix (a trailing `/v1` is removed) | `/v1/chat/completions` or `/v1/messages` |
| Codex | host/prefix including `/v1` | `/responses` |
| xAI / Grok | host/prefix including `/v1` | `/responses` (and `/responses/compact` where applicable) |

An imported operation URL such as `https://relay.example/xai/v1/responses` therefore becomes `https://relay.example/xai/v1`, avoiding duplicated paths. Query strings and userinfo in imported base URLs are rejected. Grok OAuth imports retain `refresh_token`, `team_id`, `subscription_tier`, and related metadata; custom `headers` override the default CLI identity headers case-insensitively.

**Sub2API Grok account examples:**

```json
{
  "name": "grok-oauth",
  "platform": "grok",
  "type": "oauth",
  "credentials": {
    "access_token": "xai-access-token",
    "refresh_token": "xai-refresh-token",
    "team_id": "team-123",
    "subscription_tier": "pro",
    "base_url": "https://cli-chat-proxy.grok.com/v1"
  }
}
```

```json
{
  "name": "grok-api",
  "platform": "xai",
  "type": "apikey",
  "credentials": {
    "api_key": "xai-api-key",
    "base_url": "https://relay.example/xai/v1/responses"
  }
}
```

The second example is stored as `https://relay.example/xai/v1` and the xAI Responses operation is appended by the gateway.

---

## UI

**Channels → ⋯ More → Import credentials**

- Format: Auto-detect / Sub2API data / CLIProxyAPI auth / Codex session  
- Multi-file select  
- Default group field  

Single file uses JSON API; multi-file or forced CLIProxy mode uses multipart upload.

---

## Detection rules (`format: auto`)

1. JSON `"type": "sub2api-data"` or `"sub2api-bundle"` → Sub2API data  
2. JSON has both `proxies` and `accounts` → Sub2API data  
3. JSON `"type"` in `codex|claude|gemini|gemini-cli|…` → CLIProxyAPI  
4. Flat `access_token` + `account_id` without nested `tokens` → CLIProxyAPI  
5. Otherwise → Codex session parser  

---

## Identity & safety

- Codex channels dedupe on identity keys derived from account/user/email/token fingerprint (aligned with Sub2API import).
- CLIProxy Claude/xAI re-import also uses provider-specific token, auth-file, account, refresh-token, and email identities; ambiguous matches are not updated.
- `update_existing: true` (default) refreshes credentials on a conclusive match; `false` skips.
- Batch import marks in-batch duplicates as skipped.  
- Secrets land in channel `key` (masked on list APIs); treat export files as **highly sensitive**.  
- After a successful create/update, control-api republishes the gateway snapshot.

---

## Implementation map

| Piece | Location |
|-------|----------|
| Unified import | `apps/control-api/src/control_api_ext/auth_import.rs` |
| Codex session parse | `apps/control-api/src/control_api_ext/codex_auth_import.rs` |
| Sub2API data | `apps/control-api/src/control_api_ext/sub2api_data_import.rs` |
| Base URL normalization | `apps/control-api/src/control_api_ext/upstream_url.rs` |
| xAI defaults / CLI headers | `apps/control-api/src/control_api_ext/xai_import.rs` |
| HTTP | `import_auth_json` / `import_auth_multipart` in `control_api_ext/http.rs` |
| Routes | `/api/channel/import/auth`, `/api/channel/import/auth/upload` |
| UI | `web/new-api/default/src/features/channels/components/dialogs/import-data-dialog.tsx` |
| References | `ref/sub2api`, `ref/CLIProxyAPI` |
| Porting guide (CN) | [`AUTH_FILE_PORT_CN.md`](./AUTH_FILE_PORT_CN.md) |

---

## Quick examples

**Paste Codex token (auto):**

```bash
curl -sS -b cookies.txt -H 'Content-Type: application/json' \
  -d '{"format":"auto","content":"eyJhbGciOi...","group":"default"}' \
  http://127.0.0.1:9090/api/channel/import/auth
```

**CLIProxyAPI directory of auth files:**

```bash
curl -sS -b cookies.txt \
  -F 'format=cliproxy' \
  -F 'group=default' \
  -F 'files=@auths/codex-a.json' \
  -F 'files=@auths/claude-b.json' \
  http://127.0.0.1:9090/api/channel/import/auth/upload
```

**Sub2API export file:**

```bash
curl -sS -b cookies.txt -H 'Content-Type: application/json' \
  --data-binary @export.json \
  http://127.0.0.1:9090/api/channel/import/sub2api-data
```

(or wrap as `{"content":"<file text>","group":"default"}` on `/import/auth`).

---

## Limits / non-goals (current)

- Not a full Sub2API account runtime (concurrency schedulers, Antigravity privacy hooks, etc.).  
- CLIProxy types beyond codex/claude/gemini/xai are rejected per file.  
- Gateway still uses channel keys as configured today; Codex OAuth refresh remains on control-api special routes.  
- No server-side storage of original auth filenames beyond channel remark/setting metadata.
