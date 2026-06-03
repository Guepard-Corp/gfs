#!/usr/bin/env bash
# Deterministic benchmark + correctness check for the gfs lazy-clone extension.
#
# Spins up a SOURCE PostgreSQL with a fully deterministic seed (no random()/data
# now()), makes a real `gfs clone` of it on the gfs-postgres:16 image, then for
# each query shape:
#   * asserts the CLONE result == the SOURCE result (a md5 of the ordered rows),
#   * reports the routing the gfs planner hook took -- fetched / federated / local
#     -- from the catalog counters (rows_fetched, federate_calls deltas).
# Plus two invariant checks: range elision (a re-asked range hits no source) and
# convergence (a warmed table is served local). Output and PASS/FAIL are identical
# on every run. Exit 0 on all-pass, 1 otherwise.
#
# Usage:  ./benchmark.sh [--keep]      (--keep leaves the containers up)
set -euo pipefail

EXT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$EXT_DIR/../../.." && pwd)"

GFS_BIN="${GFS_BIN:-$REPO_ROOT/target/debug/gfs}"
GFS_IMAGE="${GFS_IMAGE:-gfs-postgres:16}"
SRC_PORT="${SRC_PORT:-55470}"
CLONE_PORT="${CLONE_PORT:-55471}"
SRC_NAME="gfs-bench-src"
CLONE_DIR="$(mktemp -d)/clone"
KEEP=""; [[ "${1:-}" == "--keep" ]] && KEEP=1
PSQL="${PSQL:-psql}"   # a libpq psql on PATH (any version >= 14)

# Deterministic seed sizes (override via env). Small = fast; the laziness behaviour
# is identical at any size.
N_PRODUCTS="${N_PRODUCTS:-20000}"
N_CUSTOMERS="${N_CUSTOMERS:-2000}"
N_ORDERS="${N_ORDERS:-10000}"
N_REVIEWS="${N_REVIEWS:-8000}"

PASS=0; FAIL=0
red()  { printf '\033[31m%s\033[0m' "$1"; }
grn()  { printf '\033[32m%s\033[0m' "$1"; }
note() { printf '\n\033[1;34m== %s ==\033[0m\n' "$1"; }

cleanup() {
  [[ -n "$KEEP" ]] && { echo "kept: $SRC_NAME (:$SRC_PORT), clone (:$CLONE_PORT)"; return; }
  docker rm -f "$SRC_NAME" >/dev/null 2>&1 || true
  local c; c="$(docker ps -aq --filter "publish=${CLONE_PORT}")"
  [[ -n "$c" ]] && docker rm -f "$c" >/dev/null 2>&1 || true
  rm -rf "$(dirname "$CLONE_DIR")" 2>/dev/null || true
}
trap cleanup EXIT

src()  { PGPASSWORD=pw   "$PSQL" "postgresql://app:pw@localhost:${SRC_PORT}/appdb" -tAqc "$1"; }
cln()  { PGPASSWORD=postgres "$PSQL" "postgresql://postgres:postgres@localhost:${CLONE_PORT}/postgres" -tAqc "$1"; }

# ---------------------------------------------------------------------------
note "Preconditions"
[[ -x "$GFS_BIN" ]] || { echo "building gfs binary..."; ( cd "$REPO_ROOT" && cargo build -p gfs-cli >/dev/null 2>&1 ); }
[[ -x "$GFS_BIN" ]] || { red "gfs binary not found at $GFS_BIN\n"; exit 1; }
if ! docker image inspect "$GFS_IMAGE" >/dev/null 2>&1; then
  echo "building $GFS_IMAGE (first run, slow)..."
  docker build -t "$GFS_IMAGE" "$EXT_DIR" >/dev/null
fi
command -v "$PSQL" >/dev/null || { red "psql not on PATH (set PSQL=...)\n"; exit 1; }

# ---------------------------------------------------------------------------
note "Source: deterministic seed (products=$N_PRODUCTS orders=$N_ORDERS)"
cleanup
docker run -d --name "$SRC_NAME" -e POSTGRES_PASSWORD=pw -e POSTGRES_USER=app -e POSTGRES_DB=appdb \
  -p "${SRC_PORT}:5432" "$GFS_IMAGE" >/dev/null
for i in $(seq 1 60); do docker exec "$SRC_NAME" pg_isready -U app -d appdb >/dev/null 2>&1 && break; sleep 1; done

docker exec -i "$SRC_NAME" psql -U app -d appdb -v ON_ERROR_STOP=1 \
  -v P="$N_PRODUCTS" -v C="$N_CUSTOMERS" -v O="$N_ORDERS" -v R="$N_REVIEWS" >/dev/null <<'SQL'
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE TABLE categories (id smallint PRIMARY KEY, name text NOT NULL);
INSERT INTO categories SELECT g, (ARRAY['games','audio','books','tools','toys','kitchen',
  'garden','office','pets','video','beauty','outdoor'])[g] FROM generate_series(1,12) g;

CREATE TABLE customers (id uuid PRIMARY KEY, n bigint UNIQUE NOT NULL, email text NOT NULL, country text NOT NULL);
INSERT INTO customers SELECT md5(g::text)::uuid, g, 'c'||g||'@x.io',
  (ARRAY['FR','US','DE','JP','BR'])[1+(g%5)] FROM generate_series(1,:C) g;

CREATE TABLE products (id bigint PRIMARY KEY, name text NOT NULL,
  category_id smallint NOT NULL REFERENCES categories(id), price_cents int NOT NULL, in_stock boolean NOT NULL DEFAULT true);
INSERT INTO products SELECT g, 'Product '||g||' '||(ARRAY['crimson','delta','echo','bravo','great'])[1+(g%5)],
  1+(g%12), 100+g, (g%7)<>0 FROM generate_series(1,:P) g;
CREATE INDEX products_name_trgm ON products USING gin (name gin_trgm_ops);

CREATE TABLE orders (id bigint PRIMARY KEY, customer_id uuid NOT NULL REFERENCES customers(id),
  placed_at timestamptz NOT NULL, status text NOT NULL DEFAULT 'paid');
INSERT INTO orders SELECT g, md5((1+(g%:C))::text)::uuid, now() - ((g%365)||' days')::interval, 'paid'
  FROM generate_series(1,:O) g;
CREATE INDEX orders_placed_idx ON orders (placed_at);

CREATE TABLE order_items (order_id bigint NOT NULL REFERENCES orders(id), line int NOT NULL,
  product_id bigint NOT NULL REFERENCES products(id), qty int NOT NULL, unit_cents int NOT NULL,
  total_cents int GENERATED ALWAYS AS (qty*unit_cents) STORED, PRIMARY KEY (order_id, line));
INSERT INTO order_items SELECT g, 1, 1+(g%:P), 1+(g%5), 100+(g%50) FROM generate_series(1,:O) g;

CREATE TABLE reviews (id bigint PRIMARY KEY, product_id bigint NOT NULL REFERENCES products(id),
  customer_id uuid NOT NULL REFERENCES customers(id), rating smallint NOT NULL, body text NOT NULL);
INSERT INTO reviews SELECT g, 1+(g%:P), md5((1+(g%:C))::text)::uuid, 1+(g%5),
  'review '||(ARRAY['good','bad','okay','great','poor'])[1+(g%5)]||' '||g FROM generate_series(1,:R) g;

ANALYZE;
SQL
echo "  seeded: products=$(src 'SELECT count(*) FROM products') orders=$(src 'SELECT count(*) FROM orders')"

# ---------------------------------------------------------------------------
note "Clone"
"$GFS_BIN" clone --from "postgresql://app:pw@host.docker.internal:${SRC_PORT}/appdb" \
  --image "$GFS_IMAGE" --port "$CLONE_PORT" "$CLONE_DIR" >/dev/null 2>&1
cln "SELECT 1" >/dev/null || { red "clone unreachable\n"; exit 1; }

reset_clone() {
  cln "TRUNCATE products,categories,customers,orders,order_items,reviews;
       DELETE FROM gfs.cached; UPDATE gfs.clone_source SET whole_cached=false;
       UPDATE gfs.clone_stats SET rows_fetched=0,fetch_calls=0,federate_calls=0;" >/dev/null
}
ck() { # md5 of the ordered result of a query (set-equality across clone/source)
  echo "SELECT COALESCE(md5(string_agg(t::text,'|' ORDER BY t::text)),'NONE') FROM ($1) t"
}
counters() { cln "SELECT COALESCE(sum(rows_fetched),0)||' '||COALESCE(sum(federate_calls),0) FROM gfs.clones"; }
# query latency in ms (psql \timing) on a given side; informational (varies run to run).
time_ms() { # $1=clone|source  $2=query
  local url pw
  if [[ "$1" == clone ]]; then url="postgresql://postgres:postgres@localhost:${CLONE_PORT}/postgres"; pw=postgres
  else url="postgresql://app:pw@localhost:${SRC_PORT}/appdb"; pw=pw; fi
  PGPASSWORD="$pw" "$PSQL" "$url" -qc '\timing on' -c "$2" 2>&1 | grep -oE 'Time: [0-9.]+ ms' | head -1 | grep -oE '[0-9.]+' || echo '?'
}

SHOTS="${SHOTS:-3}"   # times each scenario is fired (no reset between shots)
TOT_HYD=0; TOT_FED=0; HDR=0

# scenario: name | expected-1st-routing | query
# Fires the query SHOTS times WITHOUT reset and shows the route each time, so the
# cache behaviour is visible: a range/key query fills the cache (fetched -> local
# -> local); a federate query does NOT cache (federated every shot -- until warmed).
scenario() {
  local name="$1" want="$2" q="$3"
  [[ $HDR == 0 ]] && { printf "  %-16s %-30s %5s %6s %-10s %s\n" \
    SCENARIO "route per shot (1->$SHOTS)" hyd rows "ms 1->$SHOTS" ok; HDR=1; }
  reset_clone
  local routes=() hyd_tot=0 fed_tot=0 ms1="" msN=""
  local s
  for s in $(seq 1 "$SHOTS"); do
    local b a; b="$(counters)"; local ms; ms="$(time_ms clone "$q")"; a="$(counters)"
    local bf bd af ad; read -r bf bd <<<"$b"; read -r af ad <<<"$a"
    local hyd=$((af-bf)) fed=$((ad-bd))
    local r="local"; [[ $fed -gt 0 ]] && r="federated"; [[ $hyd -gt 0 ]] && r="fetched"
    routes+=("$r"); hyd_tot=$((hyd_tot+hyd)); fed_tot=$((fed_tot+fed))
    [[ $s == 1 ]] && ms1="$ms"; msN="$ms"
  done
  local rows cs ss; rows="$(cln "SELECT count(*) FROM ($q) t")"
  cs="$(cln "$(ck "$q")")"; ss="$(src "$(ck "$q")")"
  # Expected: shot1 == want; subsequent shots cache iff range (fetched->local),
  # federate never caches (federated->federated).
  local exp_rest; [[ "$want" == fetched ]] && exp_rest=local || exp_rest=federated
  local pass=1
  [[ "$cs" == "$ss" ]] || pass=0
  [[ "${routes[0]}" == "$want" ]] || pass=0
  local i; for ((i=1; i<${#routes[@]}; i++)); do [[ "${routes[i]}" == "$exp_rest" ]] || pass=0; done
  TOT_HYD=$((TOT_HYD+hyd_tot)); TOT_FED=$((TOT_FED+fed_tot))
  local shotstr="" r; for r in "${routes[@]}"; do shotstr="${shotstr:+$shotstr->}$r"; done
  local ok; if [[ $pass == 1 ]]; then ok="$(grn PASS)"; PASS=$((PASS+1)); else ok="$(red FAIL)"; FAIL=$((FAIL+1)); fi
  printf "  %-16s %-30s %5s %6s %4s->%-4s %s\n" "$name" "$shotstr" "$hyd_tot" "$rows" "$ms1" "$msN" "$ok"
}

note "Scenarios -- each fired ${SHOTS}x (no reset). route per shot shows the cache filling: fetched->local (owned) vs federated->federated (never caches)"
scenario "products range"  fetched   "SELECT id,name,category_id,price_cents,in_stock FROM products WHERE id BETWEEN 1 AND 50 ORDER BY id"
scenario "fuzzy products"  federated "SELECT id,name FROM products WHERE name ILIKE '%crimson%' ORDER BY id LIMIT 50"
scenario "reviews fuzzy"   federated "SELECT id,product_id,rating FROM reviews WHERE body ILIKE '%great%' ORDER BY id LIMIT 50"
scenario "by category"     federated "SELECT p.id,p.name FROM products p JOIN categories c ON c.id=p.category_id WHERE c.name='games' ORDER BY p.id LIMIT 50"
scenario "customer orders" federated "SELECT o.id,oi.line,oi.total_cents FROM customers cu JOIN orders o ON o.customer_id=cu.id JOIN order_items oi ON oi.order_id=o.id WHERE cu.n=1 ORDER BY o.id,oi.line LIMIT 100"
scenario "recent orders"   federated "SELECT id,status FROM orders WHERE placed_at > now() - '7 days'::interval ORDER BY id LIMIT 50"
scenario "dashboard"       federated "SELECT c.name, count(*) n, sum(oi.total_cents) rev FROM order_items oi JOIN products p ON p.id=oi.product_id JOIN categories c ON c.id=p.category_id GROUP BY c.name ORDER BY c.name"
scenario "subquery agg"    federated "SELECT count(*) FROM (SELECT id FROM products WHERE name ILIKE '%bravo%' ORDER BY id LIMIT 50) t"

note "Convergence -- widening range fills + coalesces the cache; warm makes a federate table local"
reset_clone
RQB="SELECT id,name,category_id,price_cents,in_stock FROM products WHERE id BETWEEN"
cgap() { cln "SELECT COALESCE(string_agg('['||lo||','||hi||']',' ' ORDER BY lo),'NONE') FROM gfs.cached WHERE relid='public.products'::regclass"; }
nosource() { # run query, PASS iff counters unchanged (served local, 0 source); $1=label $2=query $3=extra
  local b a; b="$(counters)"; cln "$2" >/dev/null; a="$(counters)"
  if [[ "$a" == "$b" ]]; then PASS=$((PASS+1)); printf "  %-26s %s  %s\n" "$1" "$(grn 'PASS local')" "$3"
  else FAIL=$((FAIL+1)); printf "  %-26s %s  (%s -> %s)\n" "$1" "$(red FAIL)" "$b" "$a"; fi
}
cln "$RQB 1 AND 50 ORDER BY id"   >/dev/null; printf "  %-26s cached=%s\n" "shot1 [1,50] (hydrate)"      "$(cgap)"
nosource "shot2 [1,50] again"     "$RQB 1 AND 50 ORDER BY id"   "(re-ask, cached=$(cgap))"
cln "$RQB 51 AND 100 ORDER BY id" >/dev/null; printf "  %-26s cached=%s  (adjacent -> coalesced)\n" "shot3 [51,100] (hydrate)" "$(cgap)"
nosource "shot4 [40,80] span"     "$RQB 40 AND 80 ORDER BY id"  "(spans two fetches)"

reset_clone
cln "SELECT gfs.warm('public.products'); SELECT gfs.warm('public.categories');" >/dev/null
nosource "by-category after warm" "SELECT p.id FROM products p JOIN categories c ON c.id=p.category_id WHERE c.name='games' LIMIT 50" "(warmed -> owned)"

# ---------------------------------------------------------------------------
note "Stats (deterministic)"
echo "  base rows hydrated into the clone (sum  all shots): $TOT_HYD"
echo "  table-scans pushed to source (sum  all shots):      $TOT_FED  <- federate queries hit the source on EVERY shot"
printf "  source size (rows federated queries did NOT pull): products=%s orders=%s order_items=%s reviews=%s customers=%s categories=12\n" \
  "$(src 'SELECT count(*) FROM products')" "$(src 'SELECT count(*) FROM orders')" \
  "$(src 'SELECT count(*) FROM order_items')" "$(src 'SELECT count(*) FROM reviews')" "$(src 'SELECT count(*) FROM customers')"

note "Verdict (deterministic)"
echo "  $PASS passed, $FAIL failed"
if [[ "$FAIL" == 0 ]]; then grn "ALL PASS"; echo; exit 0; else red "FAILURES"; echo; exit 1; fi
