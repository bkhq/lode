// Scenario 34 — a release carries its own runtime. The manifest asset's signed
// `runtime` replaces the operator's `[runtime]` for that version: a hot update
// launches the new version under its runtime, and a rollback returns to the old
// version's runtime, still cached.

import { createHash } from "node:crypto";
import { readdirSync } from "node:fs";
import { join } from "node:path";

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";
import type { AssetRuntime } from "./helpers/sign.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

/** A stand-in runtime: prints `version` for the probe, otherwise announces itself
 *  and runs the app it was handed. */
function fakeRuntime(version: string): string {
  return `#!/bin/sh
if [ "$1" = "--version" ]; then echo "${version}"; exit 0; fi
echo "[rt] version=${version}"
exec /bin/sh "$@"
`;
}

/** Serve runtime `version` and describe it as an asset runtime. */
function serveRuntime(h: Harness, version: string): AssetRuntime {
  const body = fakeRuntime(version);
  return {
    runtime: "fakert",
    version,
    download: h.server.serveFile(`rt/${version}/fakert`, body),
    sha256: createHash("sha256").update(body).digest("hex"),
  };
}

function args(h: Harness): string[] {
  return [
    "--app",
    h.server.name,
    "--dir",
    h.dataDir,
    "--manifest",
    h.server.manifestUrl,
    "--asset",
    "app.sh",
    "--run",
    "fakert ./app.sh",
    "--log-level",
    "info",
    ...h.trustArgs("enforce"),
    "--policy",
    "off",
    "--readiness",
    "none",
    "--health-grace",
    "1",
  ];
}

test("a hot update launches the new version under its own runtime", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true, runtime: serveRuntime(h, "1.0.0") });

  const lode = h.runLodeRaw(args(h));
  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "running v0.0.1",
  });
  await lode.waitForStdout(/\[rt\] version=1\.0\.0/, { label: "v0.0.1 under runtime 1.0.0" });

  await h.publish("0.0.2", { mode: "service", latest: false, runtime: serveRuntime(h, "2.0.0") });
  await lode.requestTarget("0.0.2", (s) => s.status === "running" && s.current === "0.0.2", {
    timeout: 20000,
    label: "updated to v0.0.2",
  });
  await lode.waitForStdout(/\[rt\] version=2\.0\.0/, { label: "v0.0.2 under runtime 2.0.0" });
  await lode.waitForStdout(/\[app\] starting version=0\.0\.2/, { label: "v0.0.2 started" });

  // Both runtimes stay cached while their versions are installed.
  const keys = readdirSync(join(h.dataDir, "runtime"));
  expect(keys.filter((k) => k.startsWith("1.0.0-"))).toHaveLength(1);
  expect(keys.filter((k) => k.startsWith("2.0.0-"))).toHaveLength(1);
});

test("a rollback returns to the previous version's runtime", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true, runtime: serveRuntime(h, "1.0.0") });

  const lode = h.runLodeRaw(args(h));
  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "running v0.0.1",
  });
  await lode.waitForStdout(/\[rt\] version=1\.0\.0/, { label: "v0.0.1 under runtime 1.0.0" });

  // v0.0.2 crashes under its runtime and is rolled back.
  await h.publish("0.0.2", { mode: "exit", exitCode: 1, latest: false, runtime: serveRuntime(h, "2.0.0") });
  await lode.requestTarget(
    "0.0.2",
    (s) => s.status === "updating" || s.status === "rolling-back" || lode.countMatches(/\[rt\] version=2\.0\.0/) > 0,
    { timeout: 15000, label: "begin applying v0.0.2" },
  );
  await lode.waitForState(
    (s) => s.status === "running" && s.current === "0.0.1" && (s.history ?? []).some((e) => e.version === "0.0.2" && e.result === "bad"),
    { timeout: 20000, label: "rolled back to v0.0.1" },
  );

  expect(lode.countMatches(/\[rt\] version=2\.0\.0/)).toBeGreaterThanOrEqual(1);
  // The relaunched v0.0.1 runs under runtime 1.0.0 again.
  const relaunched = lode.stdout.slice(lode.stdout.lastIndexOf("[rt] version=2.0.0"));
  expect(relaunched).toMatch(/\[rt\] version=1\.0\.0/);
});
