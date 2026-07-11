# Halolake Auth Import

Halolake can turn **third-party credential dumps** into control-plane channels (and proxy pool entries). This is a first-class feature for operators migrating from **Sub2API** and **CLIProxyAPI**, or pasting **Codex / ChatGPT OAuth** material.

## What you can import

| Source | Input | Becomes |
|--------|--------|---------|
| **Sub2API data export** | `{ "type":"sub2api-data", "proxies":[], "accounts":[] }` | Proxy pool rows + channels |
| **CLIProxyAPI auth files** | One or more `*.json` with `"type":"codex"\|"claude"\|"gemini"…` | Channels (Codex=57, Claude=14, Gemini=24) |
| **Codex / Sub2API session** | Nested `tokens.*`, flat OAuth JSON, raw access token / multi-line mix | Codex channels (type 57) |

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
| `update_existing` | For Codex identity match: update key on existing type-57 channel |
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

Unsupported CLIProxy types (e.g. antigravity, kimi, xai) return a clear error for that file; other files in a batch still import.

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
- `update_existing: true` (default) refreshes credentials on match; `false` skips.  
- Batch import marks in-batch duplicates as skipped.  
- Secrets land in channel `key` (masked on list APIs); treat export files as **highly sensitive**.  
- After a successful create/update, control-api republishes the gateway snapshot.

---

## Implementation map

| Piece | Location |
|-------|----------|
| Unified import | `apps/control-api/src/auth_import.rs` |
| Codex session parse | `apps/control-api/src/codex_auth_import.rs` |
| Sub2API data | `apps/control-api/src/sub2api_data_import.rs` |
| HTTP | `import_auth_json` / `import_auth_multipart` in `api_channel.rs` |
| Routes | `/api/channel/import/auth`, `/api/channel/import/auth/upload` |
| UI | `web/new-api/default/src/features/channels/components/dialogs/import-data-dialog.tsx` |
| References | `ref/sub2api`, `ref/CLIProxyAPI` |

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
- CLIProxy types beyond codex/claude/gemini are rejected per file.  
- Gateway still uses channel keys as configured today; Codex OAuth refresh remains on control-api special routes.  
- No server-side storage of original auth filenames beyond channel remark/setting metadata.
