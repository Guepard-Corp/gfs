# A PostgreSQL 16 image with the GFS copy-on-read Table Access Method (the `gfs`
# pgrx extension) compiled in. GFS clones REQUIRE this extension —
# clone_bootstrap.sql runs `CREATE EXTENSION gfs` unconditionally (no overlay
# fallback), so a clone must run on an image built from this recipe.
#
#   docker build -t gfs-postgres:16 crates/extensions/gfs
#   docker run --rm -e POSTGRES_PASSWORD=x gfs-postgres:16
#
# postgres_fdw + dblink (needed by the bootstrap) ship with the official image.
# NOTE: this is a heavy multi-stage build (rustup + cargo-pgrx + a release pgrx
# build, ~10-20 min). The extension itself is validated locally via
# `cargo pgrx install`; this recipe just packages it into an image.

# ---- build stage: compile the pgrx extension against this image's pg16 -------
FROM postgres:16 AS build
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        build-essential clang libclang-dev pkg-config curl ca-certificates \
        postgresql-server-dev-16; \
    rm -rf /var/lib/apt/lists/*
# Rust toolchain + cargo-pgrx, initialized against the image's own pg16 (so the
# extension is built against the exact server it will load into).
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
RUN cargo install cargo-pgrx --version 0.18.0 --locked
RUN cargo pgrx init --pg16 "$(which pg_config)"
WORKDIR /src
COPY Cargo.toml gfs.control ./
COPY .cargo ./.cargo
COPY src ./src
RUN cargo pgrx install --release --pg-config "$(which pg_config)"

# ---- runtime stage: stock postgres:16 + the installed extension files --------
FROM postgres:16
COPY --from=build /usr/share/postgresql/16/extension/gfs.control \
                  /usr/share/postgresql/16/extension/gfs--0.0.1.sql \
                  /usr/share/postgresql/16/extension/
COPY --from=build /usr/lib/postgresql/16/lib/gfs.so /usr/lib/postgresql/16/lib/
RUN test -f /usr/share/postgresql/16/extension/gfs.control \
 && test -f /usr/lib/postgresql/16/lib/gfs.so
