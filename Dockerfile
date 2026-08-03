FROM golang:1.26-bookworm AS build
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -trimpath -o /out/keep-at ./cmd/keep-at

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
