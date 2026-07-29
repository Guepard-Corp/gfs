# A PostgreSQL image with the GFS copy-on-read Table Access Method (the `gfs`
# pgrx extension) compiled in. GFS clones REQUIRE this extension —
# clone_bootstrap.sql runs `CREATE EXTENSION gfs` unconditionally (no overlay
# fallback), so a clone must run on an image built from this recipe.
#
# The PostgreSQL major version is parameterized via PG_MAJOR (default 17).
# Build a specific version with `--build-arg`:
#
#   docker build -t gfs-postgres:17 crates/extensions/gfs
#   docker build --build-arg PG_MAJOR=16 -t gfs-postgres:16 crates/extensions/gfs
#   docker run --rm -e POSTGRES_PASSWORD=x gfs-postgres:17
#
# NOT named PG_VERSION: the official postgres image already sets
# `ENV PG_VERSION=16.14-1.pgdg13+1` (the full Debian package version), and an ENV
# from the base image overrides a same-named ARG. Building with that name asked
# apt for `postgresql-server-dev-16.14-1.pgdg13+1` and failed. BASE_IMAGE derives
# from PG_MAJOR so the two can never disagree -- previously a bare build produced
# a PostgreSQL 17 image tagged :16.
#
# postgres_fdw + dblink (needed by the bootstrap) ship with the official image.
# NOTE: this is a heavy multi-stage build (rustup + cargo-pgrx + a release pgrx
# build, ~10-20 min). The extension itself is validated locally via
# `cargo pgrx install`; this recipe just packages it into an image.
ARG PG_MAJOR=17
ARG BASE_IMAGE=postgres:${PG_MAJOR}

# ---- build stage: compile the pgrx extension against this image's postgres ----
FROM ${BASE_IMAGE} AS build
ARG PG_MAJOR
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        build-essential clang libclang-dev pkg-config curl ca-certificates \
        postgresql-server-dev-${PG_MAJOR}; \
    rm -rf /var/lib/apt/lists/*
# Rust toolchain + cargo-pgrx, initialized against the image's own postgres (so
# the extension is built against the exact server it will load into).
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
RUN cargo install cargo-pgrx --version 0.18.0 --locked
RUN cargo pgrx init --pg${PG_MAJOR} "$(which pg_config)"
WORKDIR /src
COPY Cargo.toml gfs.control ./
COPY .cargo ./.cargo
COPY src ./src
# `--no-default-features --features pg${PG_MAJOR}` so the extension is built
# for the requested major (the crate defaults to pg16).
RUN cargo pgrx install --release --no-default-features --features pg${PG_MAJOR} \
    --pg-config "$(which pg_config)"

# ---- runtime stage: stock postgres + the installed extension files -----------
FROM ${BASE_IMAGE}
ARG PG_MAJOR
COPY --from=build /usr/share/postgresql/${PG_MAJOR}/extension/gfs.control \
                  /usr/share/postgresql/${PG_MAJOR}/extension/gfs--0.0.1.sql \
                  /usr/share/postgresql/${PG_MAJOR}/extension/
COPY --from=build /usr/lib/postgresql/${PG_MAJOR}/lib/gfs.so /usr/lib/postgresql/${PG_MAJOR}/lib/
RUN test -f /usr/share/postgresql/${PG_MAJOR}/extension/gfs.control \
 && test -f /usr/lib/postgresql/${PG_MAJOR}/lib/gfs.so
