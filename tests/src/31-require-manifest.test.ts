// Scenario 31 — [trust].require_manifest on a RUNNING host. The policy must bind
// every path that reads the manifest, not only the first install: the background
// update check, an app-requested target and an on-demand check all refuse a
// manifest that does not meet it, and the running version is left alone.

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";
import { sleep } from "./helpers/util.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

/** A host running v0.0.1 from a signed catalog with a 30-day expiry. */
async function runningV1(policy: "auto" | "off", requireManifest: "signed" | "fresh") {
  h = await Harness.start();
  h.server.resign(h.signer.privPath, ["--expires-in", "30d"]);
  await h.publish("0.0.1", { mode: "service", latest: true });
  const lode = h.runLode([
    ...h.trustArgs("enforce"),
    "--require-manifest",
    requireManifest,
    "--policy",
    policy,
    "--interval",
    "1",
    "--readiness",
    "none",
    "--health-grace",
    "1",
    "--stop-timeout",
    "5",
  ]);
  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "running v0.0.1",
  });
  return lode;
}

test("policy=auto does not follow an unsigned manifest under require_manifest=signed", async () => {
  const lode = await runningV1("auto", "signed");

  // The source stops signing its catalog and advances the channel. The artifact
  // itself is properly signed, so only the manifest policy stands in the way.
  h.server.resign(undefined);
  await h.publish("0.0.2", { mode: "service", latest: true });

  const st = await lode.waitForState((s) => /require_manifest/.test(s.last_error ?? ""), {
    timeout: 15000,
    label: "update check refused",
  });
  expect(st.current).toBe("0.0.1");
  // Several more check intervals: it must keep refusing, never apply.
  await sleep(3000);
  expect(lode.readState()?.current).toBe("0.0.1");
  expect(lode.countMatches(/\[app\] starting version=0\.0\.2/)).toBe(0);
  expect(lode.exited).toBe(false);

  // Signed again: the same host now takes the update.
  h.server.resign(h.signer.privPath, ["--expires-in", "30d"]);
  const swapped = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.2", {
    timeout: 20000,
    label: "applied v0.0.2 once the manifest is signed again",
  });
  expect(swapped.current).toBe("0.0.2");
});

test("the deprecated require_manifest=fresh is signed: a signed manifest without an expiry is followed", async () => {
  const lode = await runningV1("auto", "fresh");
  expect(lode.countMatches(/require_manifest = "fresh" is deprecated/)).toBeGreaterThan(0);

  h.server.resign(h.signer.privPath);
  await h.publish("0.0.2", { mode: "service", latest: true });

  const swapped = await lode.waitForState((s) => s.status === "running" && s.current === "0.0.2", {
    timeout: 20000,
    label: "applied v0.0.2 from an undated signed manifest",
  });
  expect(swapped.current).toBe("0.0.2");
});

test("an app-requested target is not installed from an unsigned manifest", async () => {
  const lode = await runningV1("off", "signed");

  h.server.resign(undefined);
  await h.publish("0.0.2", { mode: "service", latest: true });

  const st = await lode.requestTarget("0.0.2", (s) => /require_manifest/.test(s.last_error ?? ""), {
    timeout: 15000,
    label: "target refused",
  });
  expect(st.current).toBe("0.0.1");
  expect(lode.countMatches(/\[app\] starting version=0\.0\.2/)).toBe(0);
  expect(lode.exited).toBe(false);
});

test("a tampered catalog is refused by the background check under the default policy", async () => {
  // No require_manifest at all: a catalog signature that is PRESENT must still
  // verify on this path, as it does on first install.
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });
  const lode = h.runLode([
    ...h.trustArgs("enforce"),
    "--policy",
    "auto",
    "--interval",
    "1",
    "--readiness",
    "none",
    "--health-grace",
    "1",
    "--stop-timeout",
    "5",
  ]);
  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "running v0.0.1",
  });

  // Publish v0.0.2 without moving the channel, then repoint `latest` in the
  // served file by hand: the catalog signature no longer covers it.
  await h.publish("0.0.2", { mode: "service", latest: false });
  h.server.patchServed({ channels: { stable: { latest: "0.0.2" } } });

  await lode.waitForState((s) => /signature/.test(s.last_error ?? ""), {
    timeout: 15000,
    label: "tampered catalog refused",
  });
  await sleep(2000);
  expect(lode.readState()?.current).toBe("0.0.1");
  expect(lode.countMatches(/\[app\] starting version=0\.0\.2/)).toBe(0);
});
