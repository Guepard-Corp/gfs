#!/usr/bin/env bash
set -uo pipefail
GFS_IMAGE="${GFS_IMAGE:-gfs-postgres:17}"
SRC_IMAGE="${SRC_IMAGE:-postgres:17}"
HERE="$(cd "$(dirname "$0")" && pwd)"
BOOTSTRAP="$HERE/../../adapters/db-providers/src/clone_bootstrap.sql"
RUN="gfs-privtest-$$"
LOG="$(mktemp)"
NET="$RUN-net"
PASS=0; FAIL=0
ok(){ PASS=$((PASS+1)); echo "  PASS $1"; }
bad(){ FAIL=$((FAIL+1)); echo "  FAIL $1"; }
cleanup(){ docker rm -f "$RUN-src" "$RUN-clone" >/dev/null 2>&1; docker network rm "$NET" >/dev/null 2>&1; rm -f "$LOG"; }
trap cleanup EXIT

src(){ docker exec -i "$RUN-src" psql -X -q -At -v ON_ERROR_STOP=1 -U reader -d app "$@"; }
super(){ docker exec -i "$RUN-clone" psql -X -q -At -v ON_ERROR_STOP=1 -U postgres -d postgres "$@"; }
as_user(){ local user="$1"; shift; docker exec -i -e PGPASSWORD=pw "$RUN-clone" psql -X -q -At -v ON_ERROR_STOP=1 -h 127.0.0.1 -U "$user" -d postgres "$@"; }
ready(){ for _ in $(seq 1 90); do docker exec "$1" pg_isready -h 127.0.0.1 >/dev/null 2>&1 && return 0; sleep 1; done; return 1; }

docker network create "$NET" >/dev/null
docker run -d --name "$RUN-src" --network "$NET" -e POSTGRES_USER=reader -e POSTGRES_PASSWORD=source-pw \
  -e POSTGRES_DB=app "$SRC_IMAGE" >/dev/null
docker run -d --name "$RUN-clone" --network "$NET" -e POSTGRES_PASSWORD=postgres "$GFS_IMAGE" >/dev/null
ready "$RUN-src" && ready "$RUN-clone" || { echo "containers never became ready"; exit 1; }
sleep 2

src -c "CREATE TABLE customers(id int PRIMARY KEY, name text); INSERT INTO customers VALUES (1,'a'),(2,'b'),(3,'c');
        CREATE TABLE orders(id int PRIMARY KEY); INSERT INTO orders VALUES (7),(8);
        CREATE TABLE items(id int PRIMARY KEY, label text); INSERT INTO items VALUES (1,'x'),(2,'y');" >/dev/null
docker exec "$RUN-src" pg_dump -U reader -d app --schema-only --no-owner --no-privileges | super >/dev/null 2>&1 \
  || { echo "schema replay failed"; exit 1; }

conn="host=$RUN-src port=5432 dbname=app user=reader password=source-pw"
sed -e "s/__RHOST__/$RUN-src/g" -e "s/__RPORT__/5432/g" -e "s/__RDB__/app/g" -e "s/__RUSER__/reader/g" \
    -e "s/__RPASS__/source-pw/g" -e "s/__RSSLMODE_OPT__//g" -e "s|__CONN__|$conn|g" \
    -e "s/__SCHEMAS_ARRAY__/ARRAY['public']::text[]/g" "$BOOTSTRAP" | super >"$LOG" 2>&1 \
  || { echo "clone bootstrap failed"; tail -5 "$LOG"; exit 1; }

super -c "
UPDATE gfs.cost SET negligible = 1e12, ceiling = 1e12;
CREATE ROLE owner LOGIN PASSWORD 'pw';
CREATE ROLE app_rw LOGIN PASSWORD 'pw';
CREATE ROLE app_ro LOGIN PASSWORD 'pw';
ALTER TABLE public.customers OWNER TO owner;
ALTER TABLE public.orders OWNER TO owner;
ALTER TABLE public.items OWNER TO owner;
DO \$\$ DECLARE s text; BEGIN
  FOR s IN SELECT nspname FROM pg_namespace WHERE nspname LIKE 'gfs\\_remote\\_%' LOOP
    EXECUTE format('GRANT USAGE ON SCHEMA %I TO owner, app_rw, app_ro', s);
    EXECUTE format('GRANT SELECT ON ALL TABLES IN SCHEMA %I TO owner, app_rw, app_ro', s);
  END LOOP; END \$\$;
GRANT SELECT, INSERT, UPDATE, DELETE ON public.customers, public.orders, public.items TO app_rw;
GRANT SELECT ON public.customers, public.orders, public.items TO app_ro;
CREATE TABLE public.audit_log(note text);
GRANT SELECT, INSERT ON public.audit_log TO PUBLIC;
CREATE FUNCTION public.note() RETURNS text LANGUAGE sql AS \$\$ SELECT 'legit' \$\$;
CREATE FUNCTION public.items_audit() RETURNS trigger LANGUAGE plpgsql AS \$\$
BEGIN INSERT INTO public.audit_log VALUES (note()); RETURN NEW; END \$\$;
ALTER FUNCTION public.note() OWNER TO owner;
ALTER FUNCTION public.items_audit() OWNER TO owner;
CREATE TRIGGER items_audit AFTER INSERT ON public.items FOR EACH ROW EXECUTE FUNCTION public.items_audit();
ALTER TABLE public.items ENABLE ALWAYS TRIGGER items_audit;
CREATE SCHEMA app_rw AUTHORIZATION app_rw;" >/dev/null || { echo "role setup failed"; exit 1; }
as_user app_rw -c "CREATE FUNCTION app_rw.note() RETURNS text LANGUAGE sql AS \$\$ SELECT 'planted' \$\$;
                   GRANT USAGE ON SCHEMA app_rw TO PUBLIC;" >/dev/null

echo "== hydration by readers"
[[ "$(as_user app_rw -c 'SELECT count(*) FROM customers' 2>&1)" == "3" ]] \
  && ok "a readwrite reader hydrates a cold table" || bad "a readwrite reader hydrates a cold table"
[[ "$(as_user app_ro -c 'SELECT count(*) FROM orders; SELECT current_user' 2>&1 | tr '\n' ' ')" == "2 app_ro " ]] \
  && ok "a readonly reader hydrates a cold table and stays itself" || bad "a readonly reader hydrates a cold table and stays itself"
[[ "$(super -c "SELECT whole_cached FROM gfs.clone_source WHERE relid = 'public.customers'::regclass")" == "t" ]] \
  && ok "the hydration recorded the table as wholly copied" || bad "the hydration recorded the table as wholly copied"
as_user app_rw -c "SET session_replication_role = replica" >/dev/null 2>&1 \
  && bad "the reader may not set session_replication_role" || ok "the reader may not set session_replication_role"

echo "== a trigger fired by hydration resolves names on a pinned path"
as_user app_rw -c "SET search_path = app_rw, public; SELECT count(*) FROM items" >/dev/null 2>&1
notes="$(super -c 'SELECT string_agg(note, $$,$$ ORDER BY note) FROM public.audit_log')"
[[ "$notes" == "legit,legit" ]] && ok "the ALWAYS trigger calls the owner's note(), not the one the reader planted ($notes)" \
  || bad "the ALWAYS trigger calls the owner's note(), not the one the reader planted ($notes)"

echo "== bookkeeping tables are not writable by readers"
as_user app_rw -c "INSERT INTO gfs.tombstone(relid, pk) VALUES ('public.items'::regclass, '{\"id\": 9}')" >/dev/null 2>&1 \
  && bad "a reader cannot insert a tombstone" || ok "a reader cannot insert a tombstone"
as_user app_rw -c "INSERT INTO gfs.cached(relid, lo, hi) VALUES ('public.customers'::regclass, 100, 200)" >/dev/null 2>&1 \
  && bad "a reader cannot claim coverage" || ok "a reader cannot claim coverage"
as_user app_rw -c "UPDATE gfs.clone_source SET whole_cached = true" >/dev/null 2>&1 \
  && bad "a reader cannot update clone_source" || ok "a reader cannot update clone_source"
as_user app_rw -c "DELETE FROM items WHERE id = 1" >/dev/null 2>&1 \
  && ok "a reader's DELETE still records its tombstone" || bad "a reader's DELETE still records its tombstone"
[[ "$(super -c "SELECT count(*) FROM gfs.tombstone WHERE relid = 'public.items'::regclass")" == "1" ]] \
  && ok "the tombstone is recorded" || bad "the tombstone is recorded"
[[ "$(as_user app_rw -c 'SELECT count(*) FROM customers' 2>&1)" == "3" ]] \
  && ok "a second read by the reader still serves the table" || bad "a second read by the reader still serves the table"

echo "== $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
