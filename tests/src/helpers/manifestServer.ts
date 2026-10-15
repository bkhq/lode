// A local lode/v1 manifest + artifact server (Bun.serve) over a WRITABLE temp dir.
// Everything is served fresh from disk, so publish() taking effect at runtime is
// just a file write — no caching, no restart. Binds 127.0.0.1:0 (ephemeral port),
// so the suite never touches the real network.

import { chmodSync, copyFileSync, existsSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, normalize, sep } from "node:path";

import { LODE_CLI_BIN, mkTmp, rmTmp } from "./util.ts";

// One per-version asset, keyed by its filename (`name`) — the source-agnostic
// selection key (§3) and the identity the §1 signature binds (together with the
// optional run/exec launch overrides, which override the host's [command]).
interface Asset {
  name: string;
  url: string;
  sha256: string;
  sig?: string;
  key_id?: string;
  run?: string;
  exec?: string;
  size?: number;
}

interface Manifest {
  schema: string;
  name: string;
  channels: Record<string, { latest: string }>;
  versions: Record<string, { min_lode: string; notes: string; assets: Asset[] }>;
}

export interface PublishOpts {
  artifactPath: string;
  /** The asset filename to serve — the selection key matched against
   *  `[update].asset`, and the basename the signature binds. Defaults to "app.sh". */
  name?: string;
  sha256: string;
  sig?: string;
  keyId?: string;
  /** Manifest-published launch override for bare `lode` (must match what the
   *  signature was computed over — it is a signed field). */
  run?: string;
  /** Manifest-published launch override for `lode <args>` passthrough (signed). */
  exec?: string;
  /** Point channels.stable.latest at this version (default true). */
  latest?: boolean;
}

export class ManifestServer {
  readonly name: string;
  #www: string;
  #server: ReturnType<typeof Bun.serve>;
  #manifest: Manifest;
  /** When set, every written manifest.json is signed in place via `lode-cli manifest-sign`. */
  #signKeyPath?: string;
  #signArgs: string[] = [];

  private constructor(name: string, www: string, server: ReturnType<typeof Bun.serve>, manifest: Manifest, signKeyPath?: string) {
    this.name = name;
    this.#www = www;
    this.#server = server;
    this.#manifest = manifest;
    this.#signKeyPath = signKeyPath;
  }

  /**
   * Start the server. When `signKeyPath` is given, each manifest.json is stamped
   * with a top-level `key_id` + `sig` (over the canonical message) before being
   * served, so lode's manifest-signature verification passes under
   * `require_signature = auto|enforce`.
   */
  static start(name: string, signKeyPath?: string, signArgs: string[] = []): ManifestServer {
    const www = mkTmp("lode-www-");
    const manifest: Manifest = { schema: "lode/v1", name, channels: {}, versions: {} };
    const wwwRoot = normalize(www);
    const server = Bun.serve({
      port: 0,
      hostname: "127.0.0.1",
      async fetch(req) {
        const url = new URL(req.url);
        const path = normalize(join(wwwRoot, decodeURIComponent(url.pathname)));
        // Contain serving to the www root (defeat ../ traversal).
        if (path !== wwwRoot && !path.startsWith(wwwRoot + sep)) {
          return new Response("forbidden", { status: 403 });
        }
        const file = Bun.file(path);
        if (!(await file.exists())) return new Response("not found", { status: 404 });
        return new Response(file);
      },
    });
    const ms = new ManifestServer(name, www, server, manifest, signKeyPath);
    ms.#signArgs = signArgs;
    return ms;
  }

  get url(): string {
    return `http://127.0.0.1:${this.#server.port}`;
  }

  get manifestUrl(): string {
    return `${this.url}/manifest.json`;
  }

  /** Add/replace a version: copy its artifact into the tree (served under its asset
   *  `name`, the selection key) and update manifest.json. */
  publish(version: string, opts: PublishOpts): void {
    const name = opts.name ?? "app.sh";
    const dir = join(this.#www, "artifacts", version);
    mkdirSync(dir, { recursive: true });
    // Serve the asset under its `name` so the published filename == the selection
    // key (`[update].asset`) == the basename the signature was computed over.
    copyFileSync(opts.artifactPath, join(dir, name));

    const asset: Asset = {
      name,
      url: `${this.url}/artifacts/${version}/${name}`,
      sha256: opts.sha256,
    };
    if (opts.sig) asset.sig = opts.sig;
    if (opts.keyId) asset.key_id = opts.keyId;
    if (opts.run) asset.run = opts.run;
    if (opts.exec) asset.exec = opts.exec;

    this.#manifest.versions[version] = {
      // min_lode must be satisfied by the loader under test (0.0.1).
      min_lode: "0.0.1",
      notes: `e2e ${version}`,
      assets: [asset],
    };
    if (opts.latest ?? true) {
      this.#manifest.channels.stable = { latest: version };
    }
    this.#writeManifest();
  }

  /** Serve `contents` at `path` under the www root (executable), returning its URL.
   *  Used for downloads that are not manifest assets — e.g. a stand-in `[runtime]`
   *  binary, which lode fetches straight from `[runtime].download`. */
  serveFile(path: string, contents: string): string {
    const dest = join(this.#www, path);
    mkdirSync(dirname(dest), { recursive: true });
    writeFileSync(dest, contents);
    chmodSync(dest, 0o755);
    return `${this.url}/${path}`;
  }

  /** Remove a version's served artifact bytes while leaving manifest.json intact, so
   *  any re-download 404s. Simulates a release asset going away — used to prove the
   *  loader relaunches from its local download cache without re-fetching. */
  dropArtifact(version: string, name = "app.sh"): void {
    rmSync(join(this.#www, "artifacts", version, name), { force: true });
  }

  /** Change how the catalog is signed from now on and rewrite manifest.json:
   *  `signKeyPath` undefined serves it unsigned, `extraArgs` are appended to
   *  `lode-cli manifest-sign` (e.g. `--expires-in 30d`). */
  resign(signKeyPath: string | undefined, extraArgs: string[] = []): void {
    this.#signKeyPath = signKeyPath;
    this.#signArgs = extraArgs;
    // Nothing is served before the first publish(), and an empty catalog cannot be signed.
    if (Object.keys(this.#manifest.versions).length > 0) this.#writeManifest();
  }

  /** Overwrite top-level manifest.json fields in the served file, leaving any
   *  signature as it is. For serving a catalog its signature no longer covers. */
  patchServed(fields: Record<string, unknown>): void {
    const dest = join(this.#www, "manifest.json");
    const served = JSON.parse(readFileSync(dest, "utf8"));
    const tmp = `${dest}.tmp`;
    writeFileSync(tmp, JSON.stringify({ ...served, ...fields }, null, 2));
    renameSync(tmp, dest);
  }

  #writeManifest(): void {
    if (!existsSync(this.#www)) mkdirSync(this.#www, { recursive: true });
    // Atomic, synchronous write so a concurrent lode fetch never sees a partial
    // manifest and the new content is visible the instant publish() returns. When
    // signing is enabled, the top-level key_id+sig are stamped onto the temp file
    // BEFORE the rename, so the served manifest is always atomically signed (no
    // unsigned-then-signed window for a polling lode to catch).
    const dest = join(this.#www, "manifest.json");
    const tmp = `${dest}.tmp`;
    writeFileSync(tmp, JSON.stringify(this.#manifest, null, 2));
    if (this.#signKeyPath) {
      const r = Bun.spawnSync({
        cmd: [LODE_CLI_BIN, "manifest-sign", "--into", tmp, "--key", this.#signKeyPath, ...this.#signArgs],
      });
      if (r.exitCode !== 0) {
        throw new Error(`manifest-sign failed (${r.exitCode}): ${new TextDecoder().decode(r.stderr)}`);
      }
    }
    renameSync(tmp, dest);
  }

  stop(): void {
    this.#server.stop(true);
    rmTmp(this.#www);
  }
}
