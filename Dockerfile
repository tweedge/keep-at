# Rust static build (musl): no OpenSSL needed (rustls/ring), fully
# static binary. musl target required once per host:
#   rustup target add x86_64-unknown-linux-musl
# VERSION/COMMIT stamp the binary's reported version.
FROM --platform=$BUILDPLATFORM rust:bookworm AS build
ARG VERSION=dev
ARG TARGETARCH
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && case "$TARGETARCH" in \
         amd64) TRIPLE=x86_64-unknown-linux-musl ;; \
         arm64) TRIPLE=aarch64-unknown-linux-musl ;; \
         *) echo "unsupported TARGETARCH $TARGETARCH" >&2; exit 1 ;; \
       esac \
    && rustup target add "$TRIPLE" \
    && KEEPAT_VERSION_OVERRIDE="$VERSION" \
       cargo build --release --target "$TRIPLE" \
    && cp "target/${TRIPLE}/release/keep-at" /out-keep-at

FROM alpine:3.20
RUN apk add --no-cache ca-certificates
COPY --from=build /out-keep-at /usr/local/bin/keep-at

# keep-at refuses to run without a storage limit (no default space
# consumption). Pass it as an argument to `docker run`, e.g.:
#
#   docker run -v ./data:/data -v ./storage:/storage keep-at \
#     --storage-limit 500G
#
# Or mount an advanced config file and pass --config instead.
VOLUME ["/data", "/storage"]

# `start` behaves as `run` (foreground) automatically inside a container -
# daemonizing here would just exit and kill the container. See
# daemonctl::is_containerized.
ENTRYPOINT ["keep-at", "start", "--data-dir", "/data", "--storage", "/storage"]
