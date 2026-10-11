# Built by .github/workflows/docker.yml from prebuilt release binaries.
# The build context holds linux-amd64/lode and linux-arm64/lode (extracted from
# the GitHub release tarballs); buildx sets TARGETARCH per platform.
#
# Prep stage: stage the binary once and create the `lode-cli` symlink here, so
# the final image carries ONE copy of the binary instead of two. ubase is kept
# minimal (no guaranteed shell/ln at build time), so the symlink is made in a
# digest-pinned alpine and carried over — symlinks survive COPY --from.
FROM alpine@sha256:a2d49ea686c2adfe3c992e47dc3b5e7fa6e6b5055609400dc2acaeb241c829f4 AS prep
ARG TARGETARCH
COPY linux-${TARGETARCH}/lode /staging/lode
RUN ln -s lode /staging/lode-cli

# Mini variant (`--target mini`, published as `<tag>-mini`): distroless static —
# ca-certificates, tzdata, /etc/passwd (with a `nonroot` user) and /tmp, but no libc
# and no shell, for apps that are static binaries themselves. A dynamically linked
# app or a downloaded bun/node/deno runtime cannot start here — use the slim or
# default image for those. Pinned by digest (the multi-arch index) like the others.
FROM gcr.io/distroless/static-debian13@sha256:58133991db06659feaabe0f4e97a35cebf15ef4ea08f8a4c6d2ee5f75e4aa6a0 AS mini
COPY --from=prep /staging/ /usr/bin/
ENTRYPOINT ["/usr/bin/lode"]

# Slim variant (`--target slim`, published as `<tag>-slim`): debian:13-slim — glibc,
# a shell and apt, so the glibc builds of bun/node/deno that lode downloads at boot
# still load and downstream images can `apt-get install` on top, at about a third
# of the default image's size. It ships no ca-certificates: lode bundles its TLS roots,
# an app that reads the system trust store must install them. Pinned by digest (the
# multi-arch index) like the default base below.
FROM debian:13-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f AS slim
COPY --from=prep /staging/ /usr/bin/
ENTRYPOINT ["/usr/bin/lode"]

# Default image (the last stage, built when no `--target` is given).
# Base: zzci/ubase — a general-purpose image (glibc, a shell, common tools), not a
# minimal/static one, so this same image can also host script apps whose runtime
# (bun/node/deno) lode downloads at boot into its runtime cache. lode itself is a
# musl-static binary that runs on any base, and its TLS roots are bundled
# (webpki-roots), so no system ca-certificates are required. Pinned by digest for
# reproducible image builds; the digest is the multi-arch index
# (linux/amd64 + linux/arm64) matching the platforms buildx targets below —
# re-resolve it when bumping the base.
FROM zzci/ubase@sha256:6eb4065a1481c976afc8023bb97f0386ab4b1667664d35dfbcc645feb4e0340d
# lode is a multi-call binary: as `lode` it is the loader (the entrypoint); as
# `lode-cli` (same binary, different name) it is the operator/publisher toolkit,
# reachable via `docker exec <container> lode-cli status`. Both go on PATH at
# /usr/bin so downstream images and `docker exec` can call them by bare name.
COPY --from=prep /staging/ /usr/bin/
ENTRYPOINT ["/usr/bin/lode"]
