// Scenario 31 — `[[resource]]` distribution: a config bundle published under the
// app's trust model (signed for the identity `<app>/<name>`) is installed before
// the app starts, handed to it as LODE_RESOURCE_<NAME>, updated and rolled back
// through `lode-cli pull` while the app keeps running. A bundle signed for the
// app itself (not the resource identity) is refused.

import { afterEach, expect, test } from "bun:test";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { Harness } from "./helpers/harness.ts";
import { LODE_CLI_BIN, mkTmp, rmTmp, run } from "./helpers/util.ts";

let h: Harness;
const tmps: string[] = [];

afterEach(async () => {
  await h?.dispose();
  for (const t of tmps.splice(0)) rmTmp(t);
});

interface ResourceEntry {
  current?: string;
  error?: string;
}

const resource = (s: object): ResourceEntry | undefined => (s as { resources?: Record<string, ResourceEntry> }).resources?.config;

/** Build `settings.json` for `version`, serve it, and merge it into the resource
 *  manifest signed for `identity`; returns the manifest URL. */
async function publishResource(identity: string, version: string, buildDir: string): Promise<string> {
  const dir = join(buildDir, version);
  mkdirSync(dir, { recursive: true });
  const body = JSON.stringify({ version });
  const artifact = join(dir, "settings.json");
  writeFileSync(artifact, body);
  const url = h.server.serveFile(`res/${version}/settings.json`, body);
  const manifestPath = join(buildDir, "config.json");
  const r = await run([LODE_CLI_BIN, "manifest", "--app", identity, artifact, "--version", version, "--url", url, "--key", h.signer.privPath, "--into", manifestPath]);
  if (r.exitCode !== 0) throw new Error(`lode-cli manifest failed: ${r.stderr}\n${r.stdout}`);
  return h.server.serveFile("config.json", readFileSync(manifestPath, "utf8"));
}

function writeConfig(manifestUrl: string, required: boolean): string {
  const path = join(h.dataDir, "lode.toml");
  writeFileSync(path, `[[resource]]\nname = "config"\nmanifest = "${manifestUrl}"\nasset = "settings.json"\nrequired = ${required}\n`);
  return path;
}

test("installs a resource before the app, injects its path, and pulls updates and rollbacks", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });
  const buildDir = mkTmp("lode-res-");
  tmps.push(buildDir);
  const manifestUrl = await publishResource("e2e-app/config", "1.0.0", buildDir);
  const cfgPath = writeConfig(manifestUrl, true);

  const lode = h.runLode([...h.trustArgs("enforce"), "--config", cfgPath, "--policy", "off", "--readiness", "none"]);
  const st = await lode.waitForState((s) => s.status === "running" && resource(s)?.current === "1.0.0", {
    timeout: 20000,
    label: "running with resource 1.0.0",
  });
  const active = join(h.dataDir, "resources", "config", "current", "settings.json");
  expect(JSON.parse(readFileSync(active, "utf8")).version).toBe("1.0.0");
  const environ = readFileSync(`/proc/${st.pid}/environ`, "utf8").split("\0");
  expect(environ).toContain(`LODE_RESOURCE_CONFIG=${join(h.dataDir, "resources", "config", "current")}`);

  const cli = [LODE_CLI_BIN, ...h.baseArgs(), ...h.trustArgs("enforce"), "--config", cfgPath, "pull", "--resource", "config"];
  await publishResource("e2e-app/config", "2.0.0", buildDir);
  const pulled = await run(cli);
  expect(pulled.exitCode).toBe(0);
  expect(pulled.stdout).toMatch(/requested latest/);
  await lode.waitForState((s) => resource(s)?.current === "2.0.0", { timeout: 15000, label: "resource 2.0.0" });
  expect(JSON.parse(readFileSync(active, "utf8")).version).toBe("2.0.0");

  const back = await run([...cli, "--version", "1.0.0"]);
  expect(back.exitCode).toBe(0);
  const after = await lode.waitForState((s) => resource(s)?.current === "1.0.0", { timeout: 15000, label: "rolled back to 1.0.0" });
  expect(JSON.parse(readFileSync(active, "utf8")).version).toBe("1.0.0");
  expect(after.pid).toBe(st.pid);
  expect(lode.exited).toBe(false);
});

test("a required resource signed for the app identity is refused", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });
  const buildDir = mkTmp("lode-res-");
  tmps.push(buildDir);
  // Signed (and named) for "e2e-app": valid for the app, not for the resource.
  const manifestUrl = await publishResource("e2e-app", "1.0.0", buildDir);
  const cfgPath = writeConfig(manifestUrl, true);

  const lode = h.runLode([...h.trustArgs("enforce"), "--config", cfgPath, "--policy", "off", "--readiness", "none"]);
  const exit = await lode.waitExit(20000);
  expect(exit.code).not.toBe(0);
  expect(`${lode.stdout}${lode.stderr}`).toMatch(/resource manifest is for/);
  expect(lode.countMatches(/\[app\] starting/)).toBe(0);
});
