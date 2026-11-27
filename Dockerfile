# syntax=docker/dockerfile:1
# The hashinbox server: the dashboard built with node, embedded into a static
# musl binary (apps/server/build.rs), run as non-root on bare Alpine.

# 1. The dashboard. `vite build` alone: `npm run build` also regenerates the
#    committed types and lints, which changes nothing in `dist`.
#    The UI depends on `file:../clients/typescript`: it keeps its repo path.
FROM node:22-alpine AS ui
WORKDIR /src/apps/server/ui
COPY apps/server/clients/typescript/ ../clients/typescript/
COPY apps/server/ui/package.json apps/server/ui/package-lock.json ./
RUN --mount=type=cache,target=/root/.npm npm ci
COPY apps/server/ui/ ./
RUN npx vite build

# 2. The binary, with the fresh `dist` embedded. musl links it statically.
FROM rust:1.91-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
COPY --from=ui /src/apps/server/ui/dist apps/server/ui/dist
# No .git in the context: the version comes from here, else the crate's.
ARG HASHINBOX_VERSION=
ENV CARGO_PROFILE_RELEASE_STRIP=true
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    if [ -z "$HASHINBOX_VERSION" ]; then unset HASHINBOX_VERSION; fi \
    && cargo build --locked --release -p hashinbox \
    && cp target/release/hashinbox /usr/local/bin/hashinbox

# 3. The image: CA roots for outbound HTTPS, the binary, a non-root user.
FROM alpine:3.24
LABEL org.opencontainers.image.source="https://github.com/hashinbox/hashinbox" \
      org.opencontainers.image.title="hashinbox" \
      org.opencontainers.image.description="Self-hosted Bitcoin, Ethereum and Solana wallet server with a dashboard."
RUN apk add --no-cache ca-certificates \
    && adduser -D -H -u 10001 hashinbox \
    && mkdir /data && chown hashinbox:hashinbox /data
COPY --from=build /usr/local/bin/hashinbox /usr/local/bin/hashinbox
USER hashinbox
WORKDIR /data
VOLUME /data
ENV HASHINBOX_DB=/data/hashinbox.db HASHINBOX_BIND=0.0.0.0:8080
EXPOSE 8080
# The server shuts down gracefully on Ctrl-C (SIGINT), not SIGTERM.
STOPSIGNAL SIGINT
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
    CMD wget -qO /dev/null http://127.0.0.1:8080/health || exit 1
ENTRYPOINT ["/usr/local/bin/hashinbox"]
# Plain HTTP on 0.0.0.0: a TLS-terminating proxy must sit in front.
CMD ["serve", "--tls-terminated-upstream"]
