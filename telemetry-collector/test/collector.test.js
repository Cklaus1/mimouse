import { createExecutionContext, createScheduledController, env, waitOnExecutionContext } from "cloudflare:test";
import { exports } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import example from "../example-batch.json";
import schema from "../schema.json";
import worker from "../src/index.js";
import workerSource from "../src/index.js?raw";
import { compileSchema } from "../src/validate.js";
import wranglerToml from "../wrangler.toml?raw";

const BASE = "https://collector.test";

function batch(overrides = {}) {
  return { ...structuredClone(example), install_id: crypto.randomUUID(), ...overrides };
}

function post(body, headers = {}) {
  return exports.default.fetch(`${BASE}/v1/batch`, {
    method: "POST",
    headers: { "content-type": "application/json", ...headers },
    body: typeof body === "string" ? body : JSON.stringify(body),
  });
}

function remove(installId) {
  return exports.default.fetch(`${BASE}/v1/installs/${installId}`, { method: "DELETE" });
}

async function rows(table, installId) {
  const { results } = await env.DB.prepare(`SELECT * FROM ${table} WHERE install_id = ?1`).bind(installId).all();
  return results;
}

async function expectRejected(response, status, pattern) {
  expect(response.status).toBe(status);
  expect((await response.json()).error).toMatch(pattern);
}

describe("POST /v1/batch", () => {
  it("stores a valid batch", async () => {
    const body = batch();
    const response = await post(body);
    expect(response.status).toBe(204);

    const days = await rows("days", body.install_id);
    expect(days).toHaveLength(1);
    expect(days[0]).toMatchObject({
      date: "2026-10-13",
      version: "1.0.0",
      commit_sha: "37a7126",
      os_family: "linux",
      os_version: "fedora 41",
      desktop: "GNOME",
      session_type: "wayland",
    });
    expect(JSON.parse(days[0].data)).toEqual(body.days[0]);

    const crashes = await rows("crashes", body.install_id);
    expect(crashes).toHaveLength(1);
    expect(crashes[0]).toMatchObject({
      location: "src/capture.rs:212",
      message: "<formatted>",
      capture_backend: "input-capture-portal",
      emulation_backend: "libei",
    });
  });

  it("accepts nulls, empty commit and an empty batch", async () => {
    const body = batch({ commit: "", os_version: null, desktop: null, session_type: null, crashes: [] });
    body.days[0].capture = { backend: null, selection: null };
    expect((await post(body)).status).toBe(204);
    expect(await rows("days", body.install_id)).toHaveLength(1);

    const empty = batch({ days: [], crashes: [] });
    expect((await post(empty)).status).toBe(204);
    expect(await rows("days", empty.install_id)).toHaveLength(0);
  });

  it("replaces a resent day instead of duplicating it", async () => {
    const body = batch({ crashes: [] });
    expect((await post(body)).status).toBe(204);
    body.days[0].usage.peers = 3;
    expect((await post(body)).status).toBe(204);

    const days = await rows("days", body.install_id);
    expect(days).toHaveLength(1);
    expect(JSON.parse(days[0].data).usage.peers).toBe(3);
  });

  it.each([
    ["top level", (b) => (b.hostname = "desk.lan"), "/hostname: unknown field"],
    ["day", (b) => (b.days[0].ip = "192.0.2.1"), "/days/0/ip: unknown field"],
    ["nested counters", (b) => (b.days[0].usage.switches.mouse = 1), "/days/0/usage/switches/mouse: unknown field"],
    ["crash", (b) => (b.crashes[0].path = "/home/u/x"), "/crashes/0/path: unknown field"],
    [
      "prototype key",
      (b) => Object.defineProperty(b.days[0], "__proto__", { value: {}, enumerable: true }),
      "/days/0/__proto__: unknown field",
    ],
  ])("rejects an unknown field (%s) and stores nothing", async (_, mutate, message) => {
    const body = batch();
    mutate(body);
    await expectRejected(await post(body), 400, message);
    expect(await rows("days", body.install_id)).toHaveLength(0);
    expect(await rows("crashes", body.install_id)).toHaveLength(0);
    expect(await rows("rate_limits", body.install_id)).toHaveLength(0);

    // The same client recovers with a valid batch on its next call.
    expect((await post(batch({ install_id: body.install_id }))).status).toBe(204);
    expect(await rows("days", body.install_id)).toHaveLength(1);
  });

  it.each([
    ["schema version", (b) => (b.schema_version = 2), "/schema_version: must be 1"],
    ["missing field", (b) => delete b.desktop, "/desktop: missing field"],
    ["install id", (b) => (b.install_id = "not-a-uuid"), "/install_id: invalid format"],
    ["os family", (b) => (b.os_family = "freebsd"), "/os_family: not an allowed value"],
    ["date", (b) => (b.days[0].date = "2026-02-30"), "/days/0/date: invalid date"],
    ["negative count", (b) => (b.days[0].reliability.reconnects = -1), "/days/0/reliability/reconnects: below minimum"],
    ["float count", (b) => (b.days[0].usage.peers = 1.5), "/days/0/usage/peers: expected integer"],
    ["command name", (b) => (b.days[0].usage.cli_commands["add host"] = 1), "/days/0/usage/cli_commands/add host: invalid format"],
    ["absolute path", (b) => (b.crashes[0].location = "/home/u/src/main.rs:1"), "/crashes/0/location: invalid format"],
    ["parent path", (b) => (b.crashes[0].location = "src/../../x.rs:1"), "/crashes/0/location: invalid format"],
    ["too many days", (b) => (b.days = Array(8).fill(b.days[0])), "/days: too many items"],
    ["long message", (b) => (b.crashes[0].message = "x".repeat(257)), "/crashes/0/message: too long"],
  ])("rejects an invalid %s", async (_, mutate, message) => {
    const body = batch();
    mutate(body);
    await expectRejected(await post(body), 400, message);
  });

  it("rejects bodies that are not JSON objects of the right type and size", async () => {
    await expectRejected(await post("{"), 400, /not UTF-8 JSON/);
    await expectRejected(await post("[]"), 400, /expected object/);
    await expectRejected(await post(batch(), { "content-type": "text/plain" }), 415, /content-type/);
    await expectRejected(await post(" ".repeat(64 * 1024 + 1)), 413, /larger than/);
  });

  it("never stores the client IP or request headers", async () => {
    const body = batch();
    const markers = {
      "cf-connecting-ip": "203.0.113.77",
      "x-forwarded-for": "198.51.100.23",
      "x-real-ip": "198.51.100.24",
      "user-agent": "marker-user-agent",
      cookie: "marker-cookie=1",
      referer: "https://marker-referer.example/",
    };
    expect((await post(body, markers)).status).toBe(204);

    const { results: tables } = await env.DB.prepare(
      "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '_cf_%' AND name != 'd1_migrations'",
    ).all();
    expect(tables.map((t) => t.name).sort()).toEqual(["crashes", "days", "rate_limits"]);

    // Every row this request produced, with its column names.
    let dump = "";
    for (const { name } of tables) dump += JSON.stringify(await rows(name, body.install_id));
    expect(dump).toContain(body.install_id);
    for (const [header, value] of Object.entries(markers)) {
      expect(dump).not.toContain(header);
      expect(dump).not.toContain(value);
    }
    expect(dump).not.toMatch(/\b(ip|addr|header|agent)/i);
  });

  it("rate-limits per install id and recovers the next UTC day", async () => {
    const max = Number(env.MAX_BATCHES_PER_DAY);
    const body = batch({ crashes: [] });
    for (let i = 0; i < max; i++) expect((await post(body)).status).toBe(204);

    const limited = await post(body);
    await expectRejected(limited, 429, /batches today/);
    expect(Number(limited.headers.get("retry-after"))).toBeGreaterThan(0);

    // Another install is not affected.
    expect((await post(batch())).status).toBe(204);

    // Next UTC day: move today's counter into the past, as the date change would.
    await env.DB.prepare("UPDATE rate_limits SET day = '2000-01-01' WHERE install_id = ?1").bind(body.install_id).run();
    expect((await post(body)).status).toBe(204);
  });
});

describe("DELETE /v1/installs/<id>", () => {
  it("deletes every row of that install only", async () => {
    const gone = batch();
    const kept = batch();
    expect((await post(gone)).status).toBe(204);
    expect((await post(kept)).status).toBe(204);

    const response = await remove(gone.install_id);
    expect(response.status).toBe(204);
    for (const table of ["days", "crashes", "rate_limits"]) {
      expect(await rows(table, gone.install_id)).toHaveLength(0);
      expect(await rows(table, kept.install_id)).toHaveLength(1);
    }

    // Deleting again is a no-op success, so client retries are safe.
    expect((await remove(gone.install_id)).status).toBe(204);
  });

  it("rejects malformed ids and other methods", async () => {
    await expectRejected(await remove("not-a-uuid"), 400, /invalid install id/);
    const get = await exports.default.fetch(`${BASE}/v1/installs/${crypto.randomUUID()}`);
    expect(get.status).toBe(405);
    expect(get.headers.get("allow")).toBe("DELETE");
    expect((await exports.default.fetch(`${BASE}/v1/batch`)).status).toBe(405);
    expect((await exports.default.fetch(`${BASE}/`)).status).toBe(404);
  });
});

describe("cron purge", () => {
  it("deletes rows older than 90 days and keeps newer ones", async () => {
    const old = batch();
    const recent = batch();
    expect((await post(old)).status).toBe(204);
    expect((await post(recent)).status).toBe(204);

    const now = Math.floor(Date.now() / 1000);
    for (const table of ["days", "crashes"]) {
      await env.DB.prepare(`UPDATE ${table} SET received_at = ?1 WHERE install_id = ?2`)
        .bind(now - 91 * 86400, old.install_id)
        .run();
      await env.DB.prepare(`UPDATE ${table} SET received_at = ?1 WHERE install_id = ?2`)
        .bind(now - 89 * 86400, recent.install_id)
        .run();
    }
    await env.DB.prepare("UPDATE rate_limits SET day = '2000-01-01' WHERE install_id = ?1").bind(old.install_id).run();

    const ctx = createExecutionContext();
    await worker.scheduled(createScheduledController({ scheduledTime: now * 1000, cron: "17 3 * * *" }), env, ctx);
    await waitOnExecutionContext(ctx);

    for (const table of ["days", "crashes", "rate_limits"]) {
      expect(await rows(table, old.install_id)).toHaveLength(0);
      expect(await rows(table, recent.install_id)).toHaveLength(1);
    }
  });
});

describe("contract and privacy configuration", () => {
  it("accepts the example batch", () => {
    expect(compileSchema(schema)(example)).toBeNull();
  });

  it("closes every object in the schema and requires all its fields", () => {
    const objects = [schema, ...Object.values(schema.$defs)].filter((node) => node.type === "object");
    for (const node of objects) {
      if (node.properties) {
        expect(node.additionalProperties).toBe(false);
        expect([...node.required].sort()).toEqual(Object.keys(node.properties).sort());
      } else {
        expect(node.propertyNames).toBeDefined();
      }
    }
  });

  it("refuses schema keywords it does not enforce", () => {
    expect(() => compileSchema({ type: "object", minProperties: 1 })).toThrow(/unsupported keyword minProperties/);
    expect(() => compileSchema({ type: "string", format: "email" })).toThrow(/unsupported format/);
  });

  it("does not log and has observability disabled", () => {
    expect(workerSource).not.toMatch(/console\./);
    for (const line of ["logpush = false", "[observability]", "invocation_logs = false", "persist = false"]) {
      expect(wranglerToml).toContain(line);
    }
    expect(wranglerToml).not.toMatch(/enabled = true|tail_consumers/);
  });
});
