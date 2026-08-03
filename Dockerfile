FROM golang:1.26-bookworm AS build
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -trimpath -o /out/mimis ./cmd/mimis

FROM alpine:3.20
RUN apk add --no-cache ca-certificates
COPY --from=build /out/mimis /usr/local/bin/mimis

# mimis refuses to run without at least one storage location and limit
# configured (see PLAN.md: no default space consumption). Mount a config
# file at /etc/mimisbaeti/config.yaml, or let mimis write a starter one to
# this path on first run and edit it before restarting the container.
ENV MIMIS_CONFIG=/etc/mimisbaeti/config.yaml
VOLUME ["/data", "/storage"]

# `start` behaves as `run` (foreground) automatically inside a container -
# daemonizing here would just exit and kill the container. See
# internal/daemonctl.IsContainerized.
ENTRYPOINT ["mimis", "start", "--config", "/etc/mimisbaeti/config.yaml"]
