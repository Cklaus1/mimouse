// MiMouse telemetry collector. Stores only the validated batch fields plus the
// server receive time; the client IP and request headers are never read
// into storage.

import schema from "../schema.json";
import { compileSchema } from "./validate.js";

const validateBatch = compileSchema(schema);
const INSTALL_ID = new RegExp(schema.properties.install_id.pattern, "u");
const INSTALL_PATH = /^\/v1\/installs\/([^/]+)$/;
const MAX_BODY_BYTES = 64 * 1024;
const DAY_SECONDS = 86400;
const RETENTION_SECONDS = 90 * DAY_SECONDS;

export default {
  async fetch(request, env) {
    const { pathname } = new URL(request.url);
    if (pathname === "/v1/batch") {
      return request.method === "POST" ? postBatch(request, env) : methodNotAllowed("POST");
    }
    const install = INSTALL_PATH.exec(pathname);
    if (install) {
      return request.method === "DELETE" ? deleteInstall(install[1], env) : methodNotAllowed("DELETE");
    }
    return error(404, "not found");
  },

  async scheduled(controller, env) {
    await purge(env.DB, Math.floor(controller.scheduledTime / 1000));
  },
};

async function postBatch(request, env) {
  const mediaType = (request.headers.get("content-type") ?? "").split(";")[0].trim().toLowerCase();
  if (mediaType !== "application/json") return error(415, "content-type must be application/json");

  const body = await readLimited(request.body);
  if (body === null) return error(413, `body larger than ${MAX_BODY_BYTES} bytes`);
  let batch;
  try {
    batch = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(body));
  } catch {
    return error(400, "body is not UTF-8 JSON");
  }
  const invalid = validateBatch(batch);
  if (invalid) return error(400, invalid);

  const now = Math.floor(Date.now() / 1000);
  const limited = await countBatch(env, batch.install_id, now);
  if (limited) return limited;

  const identity = [
    batch.install_id,
    now,
    batch.version,
    batch.commit,
    batch.os_family,
    batch.os_version,
    batch.desktop,
    batch.session_type,
  ];
  const statements = [
    ...batch.days.map((day) =>
      env.DB.prepare(
        `INSERT INTO days (install_id, received_at, version, commit_sha, os_family, os_version, desktop, session_type, date, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT (install_id, date) DO UPDATE SET
           received_at = excluded.received_at, version = excluded.version, commit_sha = excluded.commit_sha,
           os_family = excluded.os_family, os_version = excluded.os_version, desktop = excluded.desktop,
           session_type = excluded.session_type, data = excluded.data`,
      ).bind(...identity, day.date, JSON.stringify(day)),
    ),
    ...batch.crashes.map((crash) =>
      env.DB.prepare(
        `INSERT INTO crashes (install_id, received_at, version, commit_sha, os_family, os_version, desktop, session_type,
                              location, message, capture_backend, emulation_backend)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)`,
      ).bind(...identity, crash.location, crash.message, crash.capture_backend, crash.emulation_backend),
    ),
  ];
  if (statements.length > 0) await env.DB.batch(statements);
  return new Response(null, { status: 204, headers: { "cache-control": "no-store" } });
}

/** Counts the batch against the per-install daily limit; returns a 429 response when exceeded. */
async function countBatch(env, installId, now) {
  const max = Number(env.MAX_BATCHES_PER_DAY);
  if (!Number.isInteger(max) || max < 1) throw new Error("MAX_BATCHES_PER_DAY must be a positive integer");
  const row = await env.DB.prepare(
    `INSERT INTO rate_limits (install_id, day, count) VALUES (?1, ?2, 1)
     ON CONFLICT (install_id, day) DO UPDATE SET count = count + 1
     RETURNING count`,
  )
    .bind(installId, utcDay(now))
    .first();
  if (row.count <= max) return null;
  const retryAfter = DAY_SECONDS - (now % DAY_SECONDS);
  return error(429, `more than ${max} batches today`, { "retry-after": String(retryAfter) });
}

async function deleteInstall(installId, env) {
  if (!INSTALL_ID.test(installId)) return error(400, "invalid install id");
  await env.DB.batch(
    ["days", "crashes", "rate_limits"].map((table) =>
      env.DB.prepare(`DELETE FROM ${table} WHERE install_id = ?1`).bind(installId),
    ),
  );
  return new Response(null, { status: 204, headers: { "cache-control": "no-store" } });
}

/** Deletes rows received more than 90 days before `now` and stale rate-limit counters. */
export async function purge(db, now) {
  const cutoff = now - RETENTION_SECONDS;
  await db.batch([
    db.prepare("DELETE FROM days WHERE received_at < ?1").bind(cutoff),
    db.prepare("DELETE FROM crashes WHERE received_at < ?1").bind(cutoff),
    db.prepare("DELETE FROM rate_limits WHERE day < ?1").bind(utcDay(now)),
  ]);
}

async function readLimited(stream) {
  if (stream === null) return new Uint8Array();
  const reader = stream.getReader();
  const chunks = [];
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.byteLength;
    if (size > MAX_BODY_BYTES) {
      await reader.cancel();
      return null;
    }
    chunks.push(value);
  }
  const body = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return body;
}

function utcDay(seconds) {
  return new Date(seconds * 1000).toISOString().slice(0, 10);
}

function methodNotAllowed(allow) {
  return error(405, "method not allowed", { allow });
}

function error(status, message, headers = {}) {
  return Response.json({ error: message }, { status, headers: { "cache-control": "no-store", ...headers } });
}
