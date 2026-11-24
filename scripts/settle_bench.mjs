#!/usr/bin/env node
/**
 * Warm-open settle benchmark.
 *
 * Drives one long-lived MCP session and repeats `browser_open` against local
 * fixtures, one page at a time and one tool call at a time. For every open it
 * records:
 *   - wall-clock latency of the MCP call
 *   - SHA-256 of the returned tree (the determinism evidence: two strategies
 *     that accept the same page must return byte-identical trees)
 *   - tree character count and estimated tokens
 *   - the settle breakdown the core writes into metrics.jsonl
 *
 * The server runs with an isolated `XDG_CACHE_HOME` and a fresh metrics file,
 * so the breakdowns cannot be confused with an earlier run.
 *
 * Usage:
 *   node scripts/settle_bench.mjs --iters 30
 *   node scripts/settle_bench.mjs --iters 30 --page extras/late-hydration.html
 *   node scripts/settle_bench.mjs --iters 10 --url https://example.com
 *
 * Flags:
 *   --iters <n>      opens per page after one discarded warm-up (default 30)
 *   --url <u>        extra URL (repeatable); remote pages are not deterministic
 *   --page <name>    extra file from the fixtures dir (repeatable)
 *   --only           run only --page/--url targets, not every fixture
 *   --shadow         enable experimental settle comparison and DOM observer
 *   --fixtures <d>   fixtures dir (default crates/flashbrowse-core/tests/fixtures)
 *   --out <file>     JSON report (default settle-bench-report.json)
 *   --bin <path>     binary (default target/release/flashbrowse)
 *   --cache <dir>    isolated cache dir (default .settle-bench-cache)
 *   --port <n>       port for the fixture server (default 0, ephemeral)
 *
 * Sequential by design. Running opens in parallel would measure the machine.
 */

import { spawn, execSync } from "node:child_process";
import { createServer } from "node:http";
import { createHash } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { extname, join, resolve } from "node:path";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";

function parseArgs(argv) {
  const runTag = `${Date.now()}-${process.pid}`;
  const args = {
    iters: 30,
    urls: [],
    pages: [],
    only: false,
    shadow: false,
    fixtures: "crates/flashbrowse-core/tests/fixtures",
    out: `settle-bench-report-${runTag}.json`,
    bin: "target/release/flashbrowse",
    cache: join(tmpdir(), `flashbrowse-settle-cache-${runTag}`),
    port: 0,
  };
  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    const next = () => {
      const v = argv[++i];
      if (v === undefined) {
        console.error(`missing value for ${a}`);
        process.exit(2);
      }
      return v;
    };
    switch (a) {
      case "--iters":
        args.iters = Number(next());
        break;
      case "--url":
        args.urls.push(next());
        break;
      case "--page":
        args.pages.push(next());
        break;
      case "--only":
        args.only = true;
        break;
      case "--shadow":
        args.shadow = true;
        break;
      case "--fixtures":
        args.fixtures = next();
        break;
      case "--out":
        args.out = next();
        break;
      case "--bin":
        args.bin = next();
        break;
      case "--cache":
        args.cache = next();
        break;
      case "--port":
        args.port = Number(next());
        break;
      case "--help":
      case "-h":
        console.log(readFileSync(new URL(import.meta.url), "utf8").split("*/")[0]);
        process.exit(0);
        break;
      default:
        console.error(`unknown flag: ${a}`);
        process.exit(2);
    }
  }
  return args;
}

const args = parseArgs(process.argv);
const BIN = resolve(args.bin);
const CACHE = resolve(args.cache);
const FIXTURES = resolve(args.fixtures);

if (!existsSync(BIN)) {
  console.error(`flashbrowse binary not found: ${BIN}`);
  console.error("build it first: cargo build --release");
  process.exit(2);
}
if (!existsSync(FIXTURES)) {
  console.error(`fixtures dir not found: ${FIXTURES}`);
  process.exit(2);
}

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".htm": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".gif": "image/gif",
  ".ico": "image/x-icon",
};

// Reproduce the published warm-loop workload: 30 rows and two links per row.
const benchmarkHtml = `<!doctype html><html><head><title>Warm benchmark</title></head><body>
<h1>Warm benchmark</h1><table><thead><tr><th>ID</th><th>Name</th><th>Status</th></tr></thead><tbody>
${Array.from({ length: 30 }, (_, i) => `<tr><td>${i + 1}</td><td>Item ${i + 1}</td><td>Ready</td></tr>`).join("\n")}
</tbody></table><nav>${Array.from({ length: 60 }, (_, i) => `<a href="/item/${i + 1}">Link ${i + 1}</a>`).join(" ")}</nav>
</body></html>`;

// A fixture page may load a sibling asset; serve the whole directory.
const server = createServer((req, res) => {
  const url = new URL(req.url, "http://127.0.0.1");
  let path = decodeURIComponent(url.pathname);
  if (path === "/benchmark-30x60.html") {
    const body = Buffer.from(benchmarkHtml);
    res.writeHead(200, {
      "content-type": "text/html; charset=utf-8",
      "content-length": body.length,
      "cache-control": "no-store",
    });
    res.end(body);
    return;
  }
  if (path === "/") path = "/index.html";
  const file = resolve(join(FIXTURES, path.replace(/^\/+/, "")));
  if (!file.startsWith(FIXTURES)) {
    res.writeHead(403).end("forbidden");
    return;
  }
  try {
    const body = readFileSync(file);
    res.writeHead(200, {
      "content-type": MIME[extname(file)] ?? "application/octet-stream",
      "content-length": body.length,
      "cache-control": "no-store",
    });
    res.end(body);
  } catch {
    res.writeHead(404, { "content-length": 0 }).end();
  }
});

await new Promise((r) => server.listen(args.port, "127.0.0.1", r));
const port = server.address().port;
const base = `http://127.0.0.1:${port}`;

// Every fixture, sorted, plus explicitly requested pages and URLs. With
// `--only`, the explicit targets replace the full fixture sweep.
let pages = args.only
  ? []
  : readdirSync(FIXTURES)
      .filter((f) => /\.html?$/.test(f))
      .sort()
      .map((f) => ({
        name: f,
        url: `${base}/${f}`,
        origin: "fixture",
        deterministic: true,
      }));
if (!args.only) {
  pages.push({
    name: "benchmark-30x60.html",
    url: `${base}/benchmark-30x60.html`,
    origin: "benchmark",
    deterministic: true,
  });
}
for (const p of args.pages) {
  pages.push({
    name: p,
    url: `${base}/${p.replace(/^\/+/, "")}`,
    origin: "fixture",
    deterministic: true,
  });
}
for (const u of args.urls) {
  pages.push({ name: u, url: u, origin: "remote", deterministic: false });
}

// ---------------------------------------------------------------- client

class McpClient {
  constructor(child) {
    this.child = child;
    this.nextId = 1;
    this.pending = new Map();
    this.buffer = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => this.#onData(chunk));
    child.stderr.on("data", (d) => {
      if (process.env.SETTLE_VERBOSE) process.stderr.write(`[fb] ${d}`);
    });
  }

  #onData(chunk) {
    this.buffer += chunk;
    let idx;
    while ((idx = this.buffer.indexOf("\n")) >= 0) {
      const line = this.buffer.slice(0, idx).trim();
      this.buffer = this.buffer.slice(idx + 1);
      if (!line) continue;
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        continue;
      }
      if (msg.id !== undefined && this.pending.has(msg.id)) {
        const { res, rej } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (msg.error) rej(new Error(msg.error.message || "mcp error"));
        else res(msg.result);
      }
    }
  }

  request(method, params) {
    const id = this.nextId++;
    return new Promise((res, rej) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        rej(new Error(`timeout after 180s: ${method}`));
      }, 180_000);
      this.pending.set(id, {
        res: (v) => {
          clearTimeout(timer);
          res(v);
        },
        rej: (e) => {
          clearTimeout(timer);
          rej(e);
        },
      });
      this.child.stdin.write(
        JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n"
      );
    });
  }

  notify(method, params) {
    this.child.stdin.write(
      JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n"
    );
  }
}

function textOf(result) {
  if (!result || !Array.isArray(result.content)) return "";
  return result.content
    .filter((c) => c.type === "text")
    .map((c) => c.text)
    .join("\n");
}

// ---------------------------------------------------------------- stats

function quantile(sorted, p) {
  if (!sorted.length) return 0;
  const i = Math.min(sorted.length - 1, Math.ceil(sorted.length * p) - 1);
  return sorted[Math.max(0, i)];
}

function summarize(values) {
  const s = [...values].sort((a, b) => a - b);
  if (!s.length) return { n: 0, p50: 0, p95: 0, min: 0, max: 0, mean: 0 };
  return {
    n: s.length,
    p50: quantile(s, 0.5),
    p95: quantile(s, 0.95),
    min: s[0],
    max: s[s.length - 1],
    mean: Math.round(s.reduce((a, b) => a + b, 0) / s.length),
  };
}

const sha = (text) => createHash("sha256").update(text).digest("hex");

function sourceTreeSha256() {
  try {
    const paths = execFileSync(
      "git",
      ["ls-files", "-z", "--cached", "--others", "--exclude-standard"],
      { encoding: "utf8" },
    )
      .split("\0")
      .filter((path) => /^(crates|scripts)\//.test(path) || /^(Cargo\.toml|Cargo\.lock)$/.test(path))
      .sort();
    const hash = createHash("sha256");
    for (const path of paths) {
      hash.update(path);
      hash.update("\0");
      hash.update(readFileSync(path));
      hash.update("\0");
    }
    return hash.digest("hex");
  } catch {
    return null;
  }
}

function chromeVersion() {
  try {
    const output = execFileSync(BIN, ["doctor", "--json"], {
      encoding: "utf8",
      env: { ...process.env, XDG_CACHE_HOME: CACHE },
      stdio: ["ignore", "pipe", "ignore"],
    });
    const doctor = JSON.parse(output);
    return doctor.checks?.find((check) => check.check === "chromium")?.detail ?? null;
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------- run

rmSync(CACHE, { recursive: true, force: true });
mkdirSync(CACHE, { recursive: true });
const metricsPath = join(CACHE, "flashbrowse", "metrics.jsonl");
rmSync(metricsPath, { force: true });

const child = spawn(BIN, ["mcp", "--log-format", "json"], {
  stdio: ["pipe", "pipe", "pipe"],
  env: {
    ...process.env,
    XDG_CACHE_HOME: CACHE,
    ...(args.shadow ? { FLASHBROWSE_SETTLE_SHADOW: "1" } : {}),
  },
});

const client = new McpClient(child);
const init = await client.request("initialize", {
  protocolVersion: "2025-06-18",
  capabilities: {},
  clientInfo: { name: "settle-bench", version: "1.0.0" },
});
client.notify("notifications/initialized", {});
console.log(
  `settle bench: ${pages.length} pages x ${args.iters} opens (+1 warm-up each), ` +
    `bin ${BIN}, pid ${child.pid}, server ${base}`
);
console.log(`initialize -> ${JSON.stringify(init?.serverInfo ?? {})}`);

const startedAt = Date.now();
const results = [];
const openOrder = [];

for (const page of pages) {
  const runs = [];
  for (let i = -1; i < args.iters; i++) {
    const t0 = Date.now();
    let tree = "";
    let ok = true;
    let error = null;
    try {
      const res = await client.request("tools/call", {
        name: "browser_open",
        arguments: { url: page.url },
      });
      tree = textOf(res);
      if (res && res.isError) {
        ok = false;
        error = tree.slice(0, 300);
      }
    } catch (e) {
      ok = false;
      error = String(e).slice(0, 300);
    }
    const ms = Date.now() - t0;
    const run = {
      i,
      warmup: i < 0,
      ms,
      ok,
      error,
      chars: tree.length,
      tokens: Math.ceil(tree.length / 4),
      sha: tree ? sha(tree) : null,
    };
    runs.push(run);
    openOrder.push({ page: page.name, ...run });
    if (i % 10 === 9) {
      process.stdout.write(`  ${page.name}: ${i + 1}/${args.iters}\r`);
    }
  }
  process.stdout.write(" ".repeat(60) + "\r");
  const measured = runs.filter((r) => !r.warmup && r.ok);
  const hashes = [...new Set(measured.map((r) => r.sha))];
  const summary = {
    name: page.name,
    url: page.url,
    origin: page.origin,
    deterministic: page.deterministic,
    ok: measured.length,
    errors: runs.filter((r) => !r.ok).length,
    p50_ms: summarize(measured.map((r) => r.ms)).p50,
    p95_ms: summarize(measured.map((r) => r.ms)).p95,
    min_ms: summarize(measured.map((r) => r.ms)).min,
    max_ms: summarize(measured.map((r) => r.ms)).max,
    p50_tokens: summarize(measured.map((r) => r.tokens)).p50,
    distinct_hashes: hashes.length,
    stable: page.deterministic ? hashes.length === 1 : null,
  };
  results.push(summary);
  const flag = page.deterministic && hashes.length !== 1 ? " UNSTABLE" : "";
  console.log(
    `${page.name.padEnd(28)} p50 ${String(summary.p50_ms).padStart(5)}ms ` +
      `p95 ${String(summary.p95_ms).padStart(5)}ms ` +
      `tokens ${String(summary.p50_tokens).padStart(5)} ` +
      `hashes ${hashes.length}/${measured.length}${flag}`
  );
}

child.stdin.end();
await new Promise((r) => {
  child.on("exit", r);
  setTimeout(() => {
    child.kill("SIGTERM");
    r();
  }, 5000);
});

// Join the settle breakdowns the core wrote, in call order.
let metricRecords = [];
try {
  metricRecords = readFileSync(metricsPath, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((l) => JSON.parse(l))
    .filter((r) => r.op === "mcp:browser_open");
} catch {
  console.error(`no metrics file at ${metricsPath}`);
}

for (let i = 0; i < openOrder.length && i < metricRecords.length; i++) {
  const record = metricRecords[i];
  openOrder[i].ms_metric = record.ms;
  openOrder[i].settle = record.settle ?? null;
}

// Per-page median stage breakdown over measured opens.
const stageKeys = [
  "nav_ms",
  "poll_ms",
  "sleep_ms",
  "ax_fetch_ms",
  "compress_ms",
  "title_url_ms",
  "render_ms",
  "iterations",
  "ax_fetches",
];
for (const summary of results) {
  const rows = openOrder.filter(
    (o) => o.page === summary.name && !o.warmup && o.ok && o.settle
  );
  const stages = {};
  for (const key of stageKeys) {
    stages[key] = summarize(rows.map((r) => r.settle[key] ?? 0)).p50;
  }
  // Time inside `open` that no stage accounts for (CDP framing, loop
  // bookkeeping, the initial evaluate batch beyond its timer).
  stages.unaccounted_ms =
    summarize(
      rows.map((r) => {
        const s = r.settle;
        return Math.max(
          0,
          (r.ms_metric ?? 0) -
            (s.nav_ms +
              s.poll_ms +
              s.sleep_ms +
              s.ax_fetch_ms +
              s.compress_ms +
              s.title_url_ms +
              s.render_ms)
        );
      })
    ).p50 ?? 0;
  const exits = {};
  for (const r of rows) {
    const k = r.settle.exit_reason || "unknown";
    exits[k] = (exits[k] ?? 0) + 1;
  }
  summary.stages = stages;
  summary.exit_reasons = exits;
  const candidateAccept = rows
    .map((r) => r.settle.candidate_accept_ms)
    .filter(Number.isFinite);
  summary.candidate_accept = summarize(candidateAccept);
  summary.mutation_diagnostics = {
    delayedCandidate: rows.filter(
      (r) => r.settle.mutation_would_delay_candidate === true
    ).length,
    p50Count: summarize(rows.map((r) => r.settle.mutation_count ?? 0)).p50,
    p50QuietMs: summarize(rows.map((r) => r.settle.mutation_quiet_ms ?? 0)).p50,
  };
}

const shadow = {
  compared: 0,
  missingCandidate: [],
  divergences: [],
};
for (const row of openOrder.filter((o) => !o.warmup && o.ok)) {
  const settle = row.settle;
  if (!settle?.candidate_tree_sha256 || !settle?.legacy_tree_sha256) {
    shadow.missingCandidate.push({ page: row.page, iteration: row.i });
    continue;
  }
  shadow.compared += 1;
  if (settle.shadow_diverged) {
    shadow.divergences.push({
      page: row.page,
      iteration: row.i,
      legacy: settle.legacy_tree_sha256,
      candidate: settle.candidate_tree_sha256,
      candidateAcceptMs: settle.candidate_accept_ms,
    });
  }
}

const report = {
  generatedAt: new Date().toISOString(),
  durationSec: Math.round((Date.now() - startedAt) / 1000),
  iters: args.iters,
  binary: BIN,
  sourceRevision: (() => {
    try { return execFileSync("git", ["rev-parse", "HEAD"], { encoding: "utf8" }).trim(); }
    catch { return null; }
  })(),
  sourceTreeSha256: sourceTreeSha256(),
  settleSettings: {
    tickMs: 100,
    prefetchLeadMs: 150,
    shadow: args.shadow,
  },
  chrome: chromeVersion(),
  pages: results,
  overall: {
    opens: results.reduce((a, p) => a + p.ok, 0),
    errors: results.reduce((a, p) => a + p.errors, 0),
    p50_ms: summarize(results.map((p) => p.p50_ms)).p50,
    unstable_pages: results.filter((p) => p.stable === false).map((p) => p.name),
  },
  shadow,
  opens: openOrder,
};

writeFileSync(resolve(args.out), JSON.stringify(report, null, 2));
console.log(`\nreport: ${resolve(args.out)}`);
console.log(
  `opens ${report.overall.opens}, errors ${report.overall.errors}, ` +
    `${report.durationSec}s, unstable pages: ${
      report.overall.unstable_pages.join(", ") || "none"
    }`
);

server.close();
process.exit(report.overall.errors > 0 ? 1 : 0);
