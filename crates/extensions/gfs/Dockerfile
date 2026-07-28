# A PostgreSQL image with the GFS copy-on-read Table Access Method (the `gfs`
# pgrx extension) compiled in. GFS clones REQUIRE this extension —
# clone_bootstrap.sql runs `CREATE EXTENSION gfs` unconditionally (no overlay
# fallback), so a clone must run on an image built from this recipe.
#
# The PostgreSQL major version is parameterized via PG_VERSION (default 17).
# Build a specific version with `--build-arg`:
#
#   docker build -t gfs-postgres:17 crates/extensions/gfs
#   docker build --build-arg PG_VERSION=16 -t gfs-postgres:16 crates/extensions/gfs
#   docker run --rm -e POSTGRES_PASSWORD=x gfs-postgres:17
#
# postgres_fdw + dblink (needed by the bootstrap) ship with the official image.
# NOTE: this is a heavy multi-stage build (rustup + cargo-pgrx + a release pgrx
# build, ~10-20 min). The extension itself is validated locally via
# `cargo pgrx install`; this recipe just packages it into an image.
ARG PG_VERSION=17
ARG BASE_IMAGE=postgres:17

# ---- build stage: compile the pgrx extension against this image's postgres ----
FROM ${BASE_IMAGE} AS build
ARG PG_VERSION
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        build-essential clang libclang-dev pkg-config curl ca-certificates \
        postgresql-server-dev-${PG_VERSION}; \
    rm -rf /var/lib/apt/lists/*
# Rust toolchain + cargo-pgrx, initialized against the image's own postgres (so
# the extension is built against the exact server it will load into).
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
RUN cargo install cargo-pgrx --version 0.18.0 --locked
RUN cargo pgrx init --pg${PG_VERSION} "$(which pg_config)"
WORKDIR /src
COPY Cargo.toml gfs.control ./
COPY .cargo ./.cargo
COPY src ./src
# `--no-default-features --features pg${PG_VERSION}` so the extension is built
# for the requested major (the crate defaults to pg16).
RUN cargo pgrx install --release --no-default-features --features pg${PG_VERSION} \
    --pg-config "$(which pg_config)"

# ---- runtime stage: stock postgres + the installed extension files -----------
FROM ${BASE_IMAGE}
ARG PG_VERSION
COPY --from=build /usr/share/postgresql/${PG_VERSION}/extension/gfs.control \
                  /usr/share/postgresql/${PG_VERSION}/extension/gfs--0.0.1.sql \
                  /usr/share/postgresql/${PG_VERSION}/extension/
COPY --from=build /usr/lib/postgresql/${PG_VERSION}/lib/gfs.so /usr/lib/postgresql/${PG_VERSION}/lib/
RUN test -f /usr/share/postgresql/${PG_VERSION}/extension/gfs.control \
 && test -f /usr/lib/postgresql/${PG_VERSION}/lib/gfs.so
