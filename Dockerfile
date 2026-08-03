FROM --platform=$BUILDPLATFORM golang:1.26-bookworm AS build
ARG VERSION=dev
ARG COMMIT=unknown
# TARGETOS/TARGETARCH are set automatically by buildx per requested
# --platform. Building on $BUILDPLATFORM (the host) and cross-compiling
# via GOOS/GOARCH, instead of emulating the whole build under QEMU, is what
# makes multi-arch builds (e.g. linux/amd64 and linux/arm64 from the same
# amd64 CI runner) fast: only the final, do-nothing-but-COPY stage below
# needs the target architecture at all.
ARG TARGETOS
ARG TARGETARCH
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 GOOS=$TARGETOS GOARCH=$TARGETARCH go build -trimpath \
    -ldflags "-X github.com/tweedge/keep-at/internal/buildinfo.Version=${VERSION} -X github.com/tweedge/keep-at/internal/buildinfo.Commit=${COMMIT}" \
    -o /out/keep-at ./cmd/keep-at

FROM alpine:3.20
RUN apk add --no-cache ca-certificates
COPY --from=build /out/keep-at /usr/local/bin/keep-at

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
# internal/daemonctl.IsContainerized.
ENTRYPOINT ["keep-at", "start", "--data-dir", "/data", "--storage", "/storage"]
