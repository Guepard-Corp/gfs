# Multi-stage build for guepard-proxy-v2.
FROM rust:1-bookworm AS build
# aws-lc-rs (rustls default provider) builds its C sources with cmake.
RUN apt-get update && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY . .
RUN cargo build --release && strip target/release/guepard-proxy-v2

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/guepard-proxy-v2 /usr/local/bin/guepard-proxy-v2
# Defaults can be overridden via flags or PROXY_* env vars.
EXPOSE 6432 9090
ENTRYPOINT ["guepard-proxy-v2"]
