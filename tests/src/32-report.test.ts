// Scenario 32 — [report]: lode pushes the host's status to an OTLP/HTTP
// endpoint. A stub receiver records the exports: metrics (service identity,
// status) at start, then lifecycle events — lode.started, lode.updated after a
// hot-swap, lode.stopping on shutdown — with [report].headers attached.

import { afterEach, expect, test } from "bun:test";
import { writeFileSync } from "node:fs";
import { join } from "node:path";

import { Harness } from "./helpers/harness.ts";
import { mkTmp, rmTmp } from "./helpers/util.ts";

interface Export {
  path: string;
  auth: string | null;
  body: any;
}

let h: Harness;
let stub: ReturnType<typeof Bun.serve> | undefined;
let tmp: string | undefined;

afterEach(async () => {
  await h?.dispose();
  stub?.stop(true);
  if (tmp) rmTmp(tmp);
});

function attrs(list: { key: string; value: Record<string, string> }[]): Record<string, string> {
  return Object.fromEntries(list.map(({ key, value }) => [key, Object.values(value)[0]]));
}

function events(exports: Export[]): { name: string; attributes: Record<string, string> }[] {
  return exports
    .filter((e) => e.path === "/r/otlp/v1/logs")
    .flatMap((e) => e.body.resourceLogs[0].scopeLogs[0].logRecords)
    .map((r: any) => ({ name: r.eventName, attributes: attrs(r.attributes) }));
}

async function until(check: () => boolean, label: string, timeout = 20000): Promise<void> {
  const start = Date.now();
  while (!check()) {
    if (Date.now() - start > timeout) throw new Error(`timed out: ${label}`);
    await Bun.sleep(100);
  }
}

test("[report] exports metrics and lifecycle events over OTLP/HTTP", async () => {
  const exports: Export[] = [];
  stub = Bun.serve({
    port: 0,
    async fetch(req) {
      exports.push({ path: new URL(req.url).pathname, auth: req.headers.get("authorization"), body: await req.json() });
      return Response.json({});
    },
  });
  tmp = mkTmp("lode-report-");
  const cfgPath = join(tmp, "lode.toml");
  writeFileSync(
    cfgPath,
    `[report]
endpoint = "http://127.0.0.1:${stub.port}/r/otlp"
interval = 1
headers = ["Authorization: Bearer \${REPORT_E2E_TOKEN}"]
instance_id = "e2e-host"
attributes = { "deployment.environment.name" = "test" }
`,
  );
  process.env.REPORT_E2E_TOKEN = "pat_e2e";

  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });
  const lode = h.runLode([
    "--config",
    cfgPath,
    ...h.trustArgs("enforce"),
    "--policy",
    "check",
    "--interval",
    "1",
    "--readiness",
    "none",
    "--health-grace",
    "1",
    "--stop-timeout",
    "5",
  ]);
  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", { timeout: 20000, label: "running v0.0.1" });

  await until(
    () =>
      exports.some(
        (e) =>
          e.path === "/r/otlp/v1/metrics" &&
          e.body.resourceMetrics[0].scopeMetrics[0].metrics.some(
            (m: any) => m.name === "lode.app.status" && attrs(m.gauge.dataPoints[0].attributes).status === "running",
          ),
      ),
    "metrics reporting running",
  );
  const metrics = exports.find((e) => e.path === "/r/otlp/v1/metrics")!;
  expect(metrics.auth).toBe("Bearer pat_e2e");
  const resource = attrs(metrics.body.resourceMetrics[0].resource.attributes);
  expect(resource["service.name"]).toBe(h.server.name);
  expect(resource["service.instance.id"]).toBe("e2e-host");
  expect(resource["deployment.environment.name"]).toBe("test");
  expect(resource["lode.report.interval"]).toBe("1");
  await until(() => events(exports).some((e) => e.name === "lode.started"), "lode.started");

  await h.publish("0.0.2", { mode: "service", latest: true });
  await lode.waitForState((s) => s.available === "0.0.2", { timeout: 10000, label: "available v0.0.2" });
  await lode.requestTarget("0.0.2", (s) => s.status === "updating" || s.current === "0.0.2", { timeout: 10000, label: "target v0.0.2" });
  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.2", { timeout: 15000, label: "running v0.0.2" });
  await until(() => events(exports).some((e) => e.name === "lode.updated"), "lode.updated");
  expect(events(exports).find((e) => e.name === "lode.updated")!.attributes).toMatchObject({ from: "0.0.1", to: "0.0.2" });

  lode.signal("SIGTERM");
  await lode.waitExit(15000);
  expect(events(exports).some((e) => e.name === "lode.stopping")).toBe(true);
  const last = exports.at(-1)!;
  expect(last.path).toBe("/r/otlp/v1/metrics");
  expect(attrs(last.body.resourceMetrics[0].resource.attributes)["service.version"]).toBe("0.0.2");
});
