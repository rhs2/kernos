# Kernos kernel and control plane. Multi-stage: build with the Rust toolchain,
# run on a minimal Debian image as a non-root user with the data directory on a
# volume.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p kernos && strip target/release/kernos

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /var/lib/kernos kernos \
    && mkdir -p /var/lib/kernos && chown kernos:kernos /var/lib/kernos
COPY --from=build /src/target/release/kernos /usr/local/bin/kernos
USER kernos
ENV KERNOS_LISTEN=0.0.0.0:7401 \
    KERNOS_DATA=/var/lib/kernos \
    KERNOS_LOG=json
VOLUME ["/var/lib/kernos"]
EXPOSE 7401
HEALTHCHECK --interval=15s --timeout=3s CMD ["/usr/local/bin/kernos", "health", "--server", "http://127.0.0.1:7401"]
ENTRYPOINT ["/usr/local/bin/kernos"]
CMD ["serve"]
