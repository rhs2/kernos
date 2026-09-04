# Kernos gateway. Static Go binary on a distroless base; connectors that spawn
# MCP servers need a richer image, so this one is for the built-in connectors.
FROM golang:1.22-bookworm AS build
WORKDIR /src/gateway
COPY gateway/go.mod gateway/go.sum ./
RUN go mod download
COPY gateway ./
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /out/kernos-gateway ./cmd/kernos-gateway \
    && mkdir -p /out/data

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /out/kernos-gateway /kernos-gateway
# An empty, nonroot-owned data directory: a named volume mounted here inherits
# the ownership on first use, so the gateway can write its store and repairs.
COPY --from=build --chown=nonroot:nonroot /out/data /var/lib/kernos-gateway
ENV KERNOS_GATEWAY_LISTEN=0.0.0.0:7402 \
    KERNOS_GATEWAY_DATA=/var/lib/kernos-gateway
VOLUME ["/var/lib/kernos-gateway"]
EXPOSE 7402
USER nonroot
ENTRYPOINT ["/kernos-gateway"]
CMD ["--config", "/etc/kernos/gateway.json"]
