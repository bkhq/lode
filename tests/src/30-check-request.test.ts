// Scenario 30 — on-demand update check (integration contract §2): the app bumps
// state.check_nonce and lode checks the source right away, even under policy=off
// where it never checks by itself. lode answers by setting checked_nonce to the
// serviced nonce: `available` then holds the newer version (or is absent when up
// to date). The check only advertises — the app still applies it via state.target.

import { afterEach, expect, test } from "bun:test";

import { Harness } from "./helpers/harness.ts";
import type { LodeRunner, LodeState } from "./helpers/lode.ts";
import { sleep } from "./helpers/util.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

/** Request check `nonce` the way an app would, and wait for lode's answer. */
async function requestCheck(lode: LodeRunner, nonce: number): Promise<LodeState> {
  const start = Date.now();
  while (Date.now() - start < 10000) {
    const st = lode.readState() ?? {};
    if ((st.checked_nonce ?? 0) >= nonce) return st;
    // Re-assert: an unlocked test write can lose against lode's own state writes.
    if (st.check_nonce !== nonce) lode.writeStateField("check_nonce", nonce);
    await sleep(250);
  }
  throw new Error(`check ${nonce} was not answered; last state: ${JSON.stringify(lode.readState())}`);
}

test("policy=off: a bumped check_nonce reports the newer version without applying it", async () => {
  h = await Harness.start();
  await h.publish("0.0.1", { mode: "service", latest: true });

  const lode = h.runLode([
    ...h.trustArgs("enforce"),
    "--policy",
    "off",
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

  // Up to date: the check is answered and advertises nothing.
  const upToDate = await requestCheck(lode, 1);
  expect(upToDate.available ?? null).toBe(null);
  expect(upToDate.check_error ?? null).toBe(null);
  expect(typeof upToDate.last_check).toBe("string");

  // A newer version appears; policy=off never looks by itself.
  await h.publish("0.0.2", { mode: "service", latest: true });
  await sleep(1500);
  expect(lode.readState()?.available ?? null).toBe(null);

  const found = await requestCheck(lode, 2);
  expect(found.available).toBe("0.0.2");
  expect(found.check_error ?? null).toBe(null);
  expect(found.current).toBe("0.0.1");
  expect(found.target ?? null).toBe(null);

  // The app applies it explicitly.
  await lode.requestTarget("0.0.2", (s) => s.current === "0.0.2", {
    timeout: 20000,
    label: "request target v0.0.2",
  });
  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.2", {
    timeout: 20000,
    label: "running v0.0.2",
  });
  expect(lode.countMatches(/\[app\] starting version=0\.0\.1/)).toBe(1);
  expect(lode.exited).toBe(false);
});
