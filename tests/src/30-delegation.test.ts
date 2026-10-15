// Scenario 30 — delegated signing: lode trusts only an offline root key; a sub-key
// signs the catalog and the artifacts under a root-signed delegation embedded in
// the manifest. Revoking the sub-key through `[trust].revocation_url` makes the
// next update fail while the running version keeps serving.

import { afterEach, expect, test } from "bun:test";
import { join } from "node:path";

import { Harness } from "./helpers/harness.ts";

let h: Harness;

afterEach(async () => {
  await h?.dispose();
});

test("installs a sub-key-signed release when only the root is trusted", async () => {
  h = await Harness.start({ delegated: true });
  await h.publish("0.0.1", { mode: "service", latest: true });

  const lode = h.runLode([...h.trustArgs("enforce"), "--require-manifest", "signed", "--policy", "off", "--readiness", "none"]);
  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "running v0.0.1 (delegated signature)",
  });
  expect(lode.exited).toBe(false);
});

test("a revoked sub-key cannot install an update; v0.0.1 keeps running", async () => {
  h = await Harness.start({ delegated: true });
  await h.publish("0.0.1", { mode: "service", latest: true });
  // An empty list at first: the sub-key is still valid.
  const listPath = join(h.dataDir, "..", `${h.signer.keyId}-revocations.json`);
  await h.root!.revoke(listPath, []);
  const listUrl = h.server.serveFile("revocations.json", await Bun.file(listPath).text());

  const lode = h.runLode([...h.trustArgs("enforce"), "--revocation-url", listUrl, "--policy", "off", "--readiness", "none"]);
  await lode.waitForState((s) => s.status === "running" && s.current === "0.0.1", {
    timeout: 20000,
    label: "running v0.0.1",
  });

  await h.root!.revoke(listPath, [h.signer.keyId]);
  h.server.serveFile("revocations.json", await Bun.file(listPath).text());
  await h.publish("0.0.2", { mode: "service", latest: true });

  const st = await lode.requestTarget("0.0.2", (s) => !!s.last_error, {
    timeout: 15000,
    label: "update refused (revoked sub-key)",
  });
  expect(st.current).toBe("0.0.1");
  expect(st.last_error ?? "").toMatch(/revoked/);
  expect(lode.countMatches(/\[app\] starting version=0\.0\.2/)).toBe(0);
});
