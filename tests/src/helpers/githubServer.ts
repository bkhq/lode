// A local stand-in for the GitHub Releases API (Bun.serve on 127.0.0.1:0). It
// serves exactly the endpoints lode's GitHub adapter consumes — `/releases/latest`,
// `/releases/tags/{tag}`, `/releases` — plus the release assets themselves, so the
// `[update].github` source can be driven end to end with no real network. Point
// lode at it with `--github owner/repo --github-api <apiUrl>`.
//
// `start({ token })` makes it a private repository, as GitHub serves one: every
// API call needs `Authorization: Bearer <token>`, `browser_download_url` answers
// 404, and assets come only from `/releases/assets/<id>` with
// `Accept: application/octet-stream`, redirected to a storage host (`localhost`
// rather than `127.0.0.1`) that refuses any `Authorization` header.

import { copyFileSync, mkdirSync } from "node:fs";
import { join, normalize, sep } from "node:path";

import { mkTmp, rmTmp } from "./util.ts";

// The subset of a GitHub release-asset object the adapter reads (design §5):
// `digest` is the API's `sha256:<hex>` integrity hash, `label` the signature slot.
interface GhAsset {
  name: string;
  browser_download_url: string;
  url: string;
  digest?: string;
  label?: string;
  size?: number;
}

interface GhRelease {
  tag_name: string;
  prerelease: boolean;
  draft: boolean;
  assets: GhAsset[];
}

export interface GhPublishOpts {
  artifactPath: string;
  /** The asset filename (selection key + signed identity). Defaults to "app.sh". */
  name?: string;
  sha256: string;
  /** The §1 signature — uploaded as the asset `label` on a real release. */
  sig?: string;
  prerelease?: boolean;
  draft?: boolean;
}

export class GithubServer {
  #www: string;
  #server: ReturnType<typeof Bun.serve>;
  /** Newest first, the order GitHub's `/releases` listing uses. */
  #releases: GhRelease[] = [];
  /** Asset API ids → `<tag>/<name>` under the www root. */
  #assetIds: string[] = [];

  private constructor(www: string, server: ReturnType<typeof Bun.serve>) {
    this.#www = www;
    this.#server = server;
  }

  static start(opts: { token?: string } = {}): GithubServer {
    const www = mkTmp("lode-gh-");
    const wwwRoot = normalize(www);
    let self: GithubServer | undefined;
    const server = Bun.serve({
      port: 0,
      hostname: "127.0.0.1",
      async fetch(req) {
        const url = new URL(req.url);
        const path = url.pathname;
        const json = (body: unknown) =>
          new Response(JSON.stringify(body), { headers: { "content-type": "application/json" } });
        const releases = self?.#releases ?? [];
        const notFound = () => new Response("not found", { status: 404 });
        const authorized = req.headers.get("authorization") === `Bearer ${opts.token}`;
        if (opts.token && path.startsWith("/blob/")) {
          // The storage host: signed URL only, never a token.
          if (req.headers.get("authorization")) return new Response("only one auth mechanism allowed", { status: 400 });
          const blob = Bun.file(join(wwwRoot, "assets", decodeURIComponent(path.slice("/blob/".length))));
          return (await blob.exists()) ? new Response(blob) : notFound();
        }
        const releasesRoot = /^\/repos\/[^/]+\/[^/]+\/releases/;
        if (releasesRoot.test(path)) {
          if (opts.token && !authorized) return notFound();
          const rest = path.replace(releasesRoot, "");
          const assetId = /^\/assets\/(\d+)$/.exec(rest);
          if (assetId) {
            const target = self?.#assetIds[Number(assetId[1])];
            if (!target || req.headers.get("accept") !== "application/octet-stream") return notFound();
            const port = self?.#server.port;
            return new Response(null, { status: 302, headers: { location: `http://localhost:${port}/blob/${target}?sig=x` } });
          }
          if (rest === "/latest") {
            // GitHub's "latest": the newest non-draft, non-prerelease release.
            const latest = releases.find((r) => !r.draft && !r.prerelease);
            return latest ? json(latest) : new Response("not found", { status: 404 });
          }
          const tagged = /^\/tags\/(.+)$/.exec(rest);
          if (tagged) {
            const rel = releases.find((r) => r.tag_name === decodeURIComponent(tagged[1] ?? ""));
            return rel ? json(rel) : new Response("not found", { status: 404 });
          }
          if (rest === "") return json(releases);
          return new Response("not found", { status: 404 });
        }
        // Release assets (`browser_download_url`), contained to the www root.
        if (opts.token) return notFound();
        const file = normalize(join(wwwRoot, decodeURIComponent(path)));
        if (file !== wwwRoot && !file.startsWith(wwwRoot + sep)) {
          return new Response("forbidden", { status: 403 });
        }
        const blob = Bun.file(file);
        if (!(await blob.exists())) return new Response("not found", { status: 404 });
        return new Response(blob);
      },
    });
    self = new GithubServer(www, server);
    return self;
  }

  /** The `--github-api` base URL. */
  get apiUrl(): string {
    return `http://127.0.0.1:${this.#server.port}`;
  }

  /** Add a release for `tag` (e.g. `v0.0.2`) carrying one asset; newest first. */
  publish(tag: string, opts: GhPublishOpts): void {
    const name = opts.name ?? "app.sh";
    const dir = join(this.#www, "assets", tag);
    mkdirSync(dir, { recursive: true });
    copyFileSync(opts.artifactPath, join(dir, name));
    this.#assetIds.push(`${tag}/${name}`);
    const asset: GhAsset = {
      name,
      browser_download_url: `${this.apiUrl}/assets/${tag}/${name}`,
      url: `${this.apiUrl}/repos/_/_/releases/assets/${this.#assetIds.length - 1}`,
      digest: `sha256:${opts.sha256}`,
    };
    if (opts.sig) asset.label = opts.sig;
    this.#releases = this.#releases.filter((r) => r.tag_name !== tag);
    this.#releases.unshift({
      tag_name: tag,
      prerelease: opts.prerelease ?? false,
      draft: opts.draft ?? false,
      assets: [asset],
    });
  }

  stop(): void {
    this.#server.stop(true);
    rmTmp(this.#www);
  }
}
