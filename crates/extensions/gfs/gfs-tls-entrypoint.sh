#!/bin/sh
# Start Postgres with TLS available, then hand off to the stock entrypoint.
#
# TLS is offered, not forced: pg_hba is node-controlled (the startup seal), and
# kube-proxy SNATs NodePort traffic, so pg_hba cannot tell an internet client
# from an in-cluster one anyway. Encryption is enforced by the client —
# connection strings carry sslmode=require.
#
# Cert: a mounted pair wins (GFS_TLS_CERT_FILE/GFS_TLS_KEY_FILE, e.g. a wildcard
# for verify-full); otherwise a per-container self-signed one, which is what
# sslmode=require needs. Never baked into the image, so no shared private key.
set -eu

case "${1:-}" in
  -*) ;;
  postgres) shift ;;
  *) exec docker-entrypoint.sh "$@" ;;
esac

dir=/var/lib/gfs-tls
[ "$(id -u)" = 0 ] || dir="${TMPDIR:-/tmp}/gfs-tls"
mkdir -p "$dir"

crt="${GFS_TLS_CERT_FILE:-/etc/gfs-tls/tls.crt}"
key="${GFS_TLS_KEY_FILE:-/etc/gfs-tls/tls.key}"
if [ -r "$crt" ] && [ -r "$key" ]; then
  cp "$crt" "$dir/server.crt"
  cp "$key" "$dir/server.key"
else
  openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
    -subj "/CN=${GFS_TLS_CN:-gfs-postgres}" \
    -keyout "$dir/server.key" -out "$dir/server.crt" >/dev/null 2>&1
fi
chmod 0644 "$dir/server.crt"
chmod 0600 "$dir/server.key"
# Postgres refuses a key not owned by its own user (or root with 0640).
[ "$(id -u)" = 0 ] && chown postgres:postgres "$dir/server.crt" "$dir/server.key"

exec docker-entrypoint.sh postgres "$@" \
  -c ssl=on \
  -c "ssl_cert_file=$dir/server.crt" \
  -c "ssl_key_file=$dir/server.key"
