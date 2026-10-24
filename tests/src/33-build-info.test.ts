// Scenario 33 — lode tells the app who it is: LODE_VERSION / LODE_COMMIT /
// LODE_FEATURES in the app's environment (checked through exec passthrough) and
// the `lode` object in state.json, both matching `lode --version`.

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";
import { LODE_BIN, run } from "./helpers/util.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("the app sees lode's version, commit and features", async () => {
  // `lode 0.3.0 (0123456789ab)`, the commit part only when the build knew it.
  const banner = (await run([LODE_BIN, "--version"])).stdout.trim();
  const [, version, short] = banner.match(/^lode (\S+)(?: \(([0-9a-f]+)\))?$/) ?? [];
  expect(version).toBeTruthy();

  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  const exec = h.runLode([...h.trustArgs("enforce"), "--policy", "off", "lode-env"]);
  expect((await exec.waitExit(20000)).code).toBe(0);
  const env = Object.fromEntries(
    exec.stdout
      .split("\n")
      .filter((l) => l.startsWith("LODE_"))
      .map((l) => [l.slice(0, l.indexOf("=")), l.slice(l.indexOf("=") + 1)]),
  );
  expect(env.LODE_VERSION).toBe(version);
  if (short) expect(env.LODE_COMMIT.startsWith(short)).toBe(true);
  const features = env.LODE_FEATURES.split(",");
  expect(features).toContain("target");
  expect(features).toContain("report");

  const lode = h.runLode([...h.trustArgs("enforce"), "--policy", "off", "--readiness", "none", "--health-grace", "1"]);
  const st = await lode.waitForState((s) => s.status === "running" && s.lode !== undefined, {
    timeout: 20000,
    label: "running with lode info",
  });
  expect(st.lode!.version).toBe(version);
  expect(st.lode!.commit ?? "").toBe(env.LODE_COMMIT);
  expect(st.lode!.features).toEqual(features);
});
