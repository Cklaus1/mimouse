# MiMouse telemetry collector

Cloudflare Worker + D1 that receives MiMouse's opt-in telemetry. The client
only talks to it after the user ran `mimouse cli telemetry on`.

## API

| Route | Result |
|---|---|
| `POST /v1/batch` | Body: one JSON batch matching [`schema.json`](schema.json), `content-type: application/json`, at most 64 KiB. `204` stored; `400` invalid (unknown or missing field, wrong `schema_version`, bad value; the body names the first offending JSON pointer); `413` too large; `415` wrong content type; `429` more than `MAX_BATCHES_PER_DAY` batches for this install id today (UTC), with `retry-after` in seconds. |
| `DELETE /v1/installs/<install_id>` | Deletes every row of that install id. `204` (also when nothing was stored); `400` if the id is not a UUID v4. |

Errors are `{"error": "<message>"}`.

## Contract

[`schema.json`](schema.json) (JSON Schema 2020-12) is the contract for the
Rust client and the only source of validation: the Worker interprets it at
runtime (`src/validate.js`), so editing the schema changes what is accepted.
Every object lists its complete field set; all fields are required (send
`null` for the nullable ones) and any other field is rejected.
[`example-batch.json`](example-batch.json) is a valid batch.

Fields of schema version 1:

- `schema_version`: `1`.
- `install_id`: random UUID v4, lowercase.
- `version`: `PKG_VERSION`. `commit`: `SHORT_COMMIT`, empty outside git.
- `os_family`: `linux` | `windows` | `macos`. `os_version`: string or null.
- `desktop`: `XDG_CURRENT_DESKTOP` or null. `session_type`: `XDG_SESSION_TYPE` or null.
- `days[]` (completed UTC days, at most 7), each with:
  - `date`: `YYYY-MM-DD`.
  - `capture`, `emulation`: `backend` (config backend name or null) and `selection` (`configured` | `auto` | `fallback` | null).
  - `reliability`: `reconnects`, `ping_timeouts`, `watchdog_releases`, `capture_disables`, `emulation_disables`, `clipboard_failures` (counts); `approval_waits` (counts by duration: `lt_10s`, `lt_60s`, `ge_60s`).
  - `usage`: `switches` (`edge`, `hotkey`, `cli_focus`); `peers` (configured peers); `clipboard_transfers` and `file_transfers` (counts by size: `lt_1k`, `lt_64k`, `lt_1m`, `lt_64m`, `ge_64m`); `cli_commands` (IPC request name in snake_case to count).
- `crashes[]` (at most 8), each with `location` (`file.rs:line`, relative), `message` (static panic message or `<formatted>`, at most 256 characters), `capture_backend`, `emulation_backend`.

A new field means bumping `schema_version` (the `const` in `schema.json`),
this list, the main README list and the client's no-PII test together.

## Privacy

- Only the validated batch fields plus the server receive time
  (`received_at`) are stored. The Worker never reads the client IP or
  request headers into storage and never logs (no `console.*`).
- `wrangler.toml` disables Workers Logs, invocation logs, traces, Logpush and
  preview URLs. Do not add `tail_consumers` or enable observability.
- Rows older than 90 days are deleted by the daily cron trigger
  (`17 3 * * *` UTC), together with old rate-limit counters.
- `DELETE /v1/installs/<id>` (sent by `mimouse cli telemetry off`) removes
  everything stored for that install.

## Storage

`migrations/0001_init.sql`: `days` (one row per install and day, the day
object as JSON in `data`, identity fields as columns), `crashes`, and
`rate_limits` (accepted batches per install and UTC day).

## Develop and test

Requires Node 22 and npm.

```sh
npm ci
npm test          # vitest in the Workers runtime (Miniflare), local D1
npm run dev       # applies migrations to a local D1, then wrangler dev on :8787
curl -i -H 'content-type: application/json' --data @example-batch.json http://127.0.0.1:8787/v1/batch
curl -i -X DELETE http://127.0.0.1:8787/v1/installs/3f2b8c1e-9a4d-4e7f-8b21-6c0d5e9a1f34
curl -i http://127.0.0.1:8787/cdn-cgi/local/scheduled   # runs the purge
```

Set `WRANGLER_SEND_METRICS=false` to stop wrangler's own usage reporting.

## Deploy (needs a human)

Needs a Cloudflare account and a domain on it.

1. `npx wrangler login`
2. `npx wrangler d1 create mimouse-telemetry` and put the printed
   `database_id` into `wrangler.toml`.
3. In `wrangler.toml`, uncomment `routes` with your hostname (for example
   `telemetry.example.org`, `custom_domain = true`) and `workers_dev = false`.
4. `npm run deploy` (applies migrations to the remote D1, then deploys).
5. In the Cloudflare dashboard, check that Workers Logs and Logpush are off
   for the Worker.
6. Smoke: post `example-batch.json` and `DELETE` its install id as in
   "Develop and test", against `https://<hostname>`.
7. Set the repository secret `MIMOUSE_TELEMETRY_URL` to the base URL
   `https://<hostname>` for tag builds; the client appends `/v1/batch` and
   `/v1/installs/<id>`.
