# The web image, built from an ASSEMBLED release tree (#922, #952; deploy/tools/assemble-release.sh).
# Since the repo split cutover this is the only web image recipe: apps/web/Dockerfile is gone, and
# the last CI run proving both built the same image is linked from the cutover PR. Build context =
# the extracted tarball root (noevia/, web/, core/, release-refs), e.g.
#   docker build -f noevia/build/web.Dockerfile --build-arg COWORK_VERSION=<sha> <tree>
# Releases use deploy/tools/build-web-release.sh, which wraps exactly this.
# dav-parse.wasm (#967): the Rust/WebAssembly port of core's PROPFIND listing parser, used only with
# DAV_PARSE_IMPL=wasm. core/server/dav-parse.lock pins the noevia-rs ref, its tarball sha256 and the
# module sha256; the tarball is checked before anything is built and the module after.
FROM node:22-alpine AS dav-parse-src
COPY core/server/dav-parse.lock /lock
RUN set -e; eval "$(grep -E '^(NOEVIA_RS_REF|NOEVIA_RS_SHA256)=[0-9a-f]+$' /lock)"; \
    node -e 'fetch(process.argv[1]).then(r=>{if(!r.ok)throw new Error("HTTP "+r.status);return r.arrayBuffer()}).then(b=>require("fs").writeFileSync("/rs.tgz",Buffer.from(b))).catch(e=>{console.error(e.message);process.exit(1)})' \
      "https://codeload.github.com/sbstndalton/noevia-rs/tar.gz/$NOEVIA_RS_REF"; \
    echo "$NOEVIA_RS_SHA256  /rs.tgz" | sha256sum -c -; \
    mkdir /rs && tar -xzf /rs.tgz --strip-components=1 -C /rs && rm /rs.tgz

FROM rust:1.99-slim-bookworm AS dav-parse
ENV RUSTUP_TOOLCHAIN=1.99.0 CARGO_TERM_COLOR=never
COPY --from=dav-parse-src /rs /src
COPY core/server/dav-parse.lock /lock
WORKDIR /src
RUN set -e; rustup target add wasm32-unknown-unknown; \
    tools/build-dav-parse-wasm.sh /out/dav-parse.wasm; \
    echo "$(sed -n 's/^DAV_PARSE_WASM_SHA256=//p' /lock)  /out/dav-parse.wasm" | sha256sum -c -

# The Rust front (full-Rust migration M1): noevia-rs bins/noevia-server, from the tree's rs/ (the
# noevia-rs tarball assemble-release.sh fetched at release/versions.lock's NOEVIA_RS_REF and
# checked against NOEVIA_RS_SHA256). Used only with NOEVIA_FRONT=rust (build/web-supervisor.sh).
# A NOEVIA_RS_REF from before bins/noevia-server, or a tree from a lock with no NOEVIA_RS_SHA256,
# builds an image without it; NOEVIA_FRONT=rust then refuses to start.
FROM rust:1.99-slim-bookworm AS front
ENV RUSTUP_TOOLCHAIN=1.99.0 CARGO_TERM_COLOR=never
COPY rs/ /src/
WORKDIR /src
RUN set -e; mkdir -p /out; \
    if [ -f bins/noevia-server/Cargo.toml ]; then \
      features=rust-reasoning-settings,rust-routing-mode; \
      if grep -qx 'rust-project-chats = \[\]' bins/noevia-server/Cargo.toml; then features="$features,rust-project-chats"; fi; \
      if grep -qx 'rust-free-chats = \[\]' bins/noevia-server/Cargo.toml; then features="$features,rust-free-chats"; fi; \
      if grep -qx 'rust-project-writes = \[\]' bins/noevia-server/Cargo.toml; then features="$features,rust-project-writes"; fi; \
      if grep -qx 'rust-routing-default = \[\]' bins/noevia-server/Cargo.toml; then features="$features,rust-routing-default"; fi; \
      if grep -qx 'rust-stats = \[\]' bins/noevia-server/Cargo.toml; then features="$features,rust-stats"; fi; \
      if grep -q '^rust-storage-reads = ' bins/noevia-server/Cargo.toml; then features="$features,rust-storage-reads"; fi; \
      if grep -q '^rust-storage-browse = ' bins/noevia-server/Cargo.toml; then features="$features,rust-storage-browse"; fi; \
      if grep -qx 'rust-export = \[\]' bins/noevia-server/Cargo.toml; then features="$features,rust-export"; fi; \
      if grep -q '^rust-chatgpt-status = ' bins/noevia-server/Cargo.toml; then features="$features,rust-chatgpt-status"; fi; \
      cargo build --release --locked -p noevia-server --features "$features"; \
      cp target/release/noevia-server /out/noevia-server; \
      /out/noevia-server 2>&1 | grep -q NOEVIA_LEGACY_UPSTREAM; \
    else \
      echo "noevia-rs in this tree has no bins/noevia-server: the image ships without the Rust front"; \
    fi

FROM node:22-alpine AS build
WORKDIR /app
# COWORK_VERSION stays the noevia (integration repo) sha; it must match the assembled tree's
# release-refs NOEVIA_SHA. Optional here: the tree already names its sha.
ARG COWORK_VERSION
ARG REQUIRE_RELEASE_VERSION=0
COPY release-refs ./
RUN set -e; . ./release-refs; \
    if [ "$REQUIRE_RELEASE_VERSION" = 1 ] && { [ -z "$COWORK_VERSION" ] || [ "$COWORK_VERSION" = dev ]; }; then \
      echo "COWORK_VERSION build-arg is required for a release build" >&2; exit 1; fi; \
    if [ -n "$COWORK_VERSION" ] && [ "$COWORK_VERSION" != dev ] && [ "$COWORK_VERSION" != "$NOEVIA_SHA" ]; then \
      echo "COWORK_VERSION=$COWORK_VERSION but the assembled tree is noevia $NOEVIA_SHA" >&2; exit 1; fi
COPY web/package.json web/package-lock.json ./
# Prefer IPv4 for registry lookups: hosts with a broken IPv6 route otherwise time out.
RUN NODE_OPTIONS=--dns-result-order=ipv4first npm ci --no-fund --no-audit
# Lay web/ and core/ out as the monorepo's apps/web was, so the same test gate and build command run.
COPY web/ ./
COPY core/server ./server
# #2069: preserved JS oracles are test inputs too. Older core pins have no
# reference directory; create the empty destination so the runtime COPY works for both.
RUN mkdir -p tests/reference
COPY core/tests ./tests
COPY --from=dav-parse /out/dav-parse.wasm ./server/wasm/dav-parse.wasm
# #1279: finish both Rust compiler stages before the suite and isolated timing phase.
COPY --from=front /out/ /tmp/qualified-front/
# DAV_PARSE_WASM_REQUIRED: the dav-parse differential test must run against the module, not skip.
RUN set -e; set -a; . ./release-refs; set +a; export STAMP_VERSION="$NOEVIA_SHA" DAV_PARSE_WASM_REQUIRED=1; \
    node --test --test-concurrency=1 --test-timeout=120000 tests/client/*.test.cjs tests/server/*.test.cjs && \
    node --test --test-concurrency=1 tests/server/code-actions-performance.qualification.cjs && npm run build && \
    node -e 'const fs=require("fs");const f="dist/version.json";const v=JSON.parse(fs.readFileSync(f,"utf8"));if(v.version!==process.env.STAMP_VERSION){console.error("version.json not stamped with "+process.env.STAMP_VERSION);process.exit(1)}fs.writeFileSync(f,JSON.stringify({version:v.version,web:process.env.NOEVIA_WEB_SHA,core:process.env.NOEVIA_CORE_SHA})+"\n")'

# Runtime (glibc for sqlite-vec / better-sqlite3).
FROM node:22-bookworm
WORKDIR /app
COPY --from=build /app/dist ./dist
# /api/ready reads dist/version.json; package.json (web's) is the fallback (#337).
COPY web/package.json ./
COPY core/server/package.json core/server/package-lock.json* ./server/
RUN cd server && NODE_OPTIONS=--dns-result-order=ipv4first npm ci --no-fund --no-audit
COPY core/server ./server
COPY --from=dav-parse /out/dav-parse.wasm ./server/wasm/dav-parse.wasm
# core owns contracts/; server/ requires it as ../contracts.
COPY core/contracts ./contracts
# #1990, #2077: in-image server qualification needs the project-store stand-ins and
# synthetic account seeder from ../tests/server. Copy only these fixtures, rather
# than the rest of core/tests/server; the legacy-claim test must run without skips.
COPY core/tests/server/project-store-stub.cjs core/tests/server/project-store-server.cjs core/tests/server/seed-account.cjs ./tests/server/
# Required by differential/selector tests mounted into the shipped runtime at qualification.
COPY --from=build /app/tests/reference ./tests/reference
# noevia-server on UI_PORT and Node on loopback UI_PORT+1000 (build/web-supervisor.sh). Since
# sbstndalton/noevia#1071 that is the only front, and it owns sign-in, the project routes and the
# store with no switch; the supervisor refuses to start without it. The rs-less build leaves
# /out empty and the image without the binary, which the supervisor refuses too.
COPY --from=front /out/ /usr/local/bin/
COPY noevia/build/web-supervisor.sh /app/web-supervisor.sh
ENV UI_PORT=8021 NOEVIA_FRONT=rust NOEVIA_WEB_DIST=/app/dist
EXPOSE 8021
HEALTHCHECK --interval=30s --timeout=6s --start-period=30s --retries=3 CMD node -e "fetch('http://127.0.0.1:'+(process.env.UI_PORT||8021)+'/api/setup/status',{signal:AbortSignal.timeout(5000)}).then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"
CMD ["bash", "/app/web-supervisor.sh"]
