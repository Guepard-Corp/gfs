# Shared TPC-H source provisioning for the benchmark — sourced by BOTH front-ends
# (scripts/run.sh for the UI, scripts/bench.sh for the headless runner) so the
# source is set up ONE way. Generates TPC-H with DuckDB, loads it WITH PRIMARY KEYS
# (the clone bootstrap only registers tables that have a unique index) into a tuned
# postgres:16 on a NAMED VOLUME (per scale factor), and REUSES it across runs — only
# the lazy clone is rebuilt.
#
# Expects these to be set by the caller before `ensure_source`:
#   SF SFTAG REPO_ROOT GFS_BIN GFS_IMAGE PSQL SOURCE_PORT SRC_NAME SRC_VOL DATA_DIR
# Honors DROP_SOURCE=1 (delete container+volume, exit) and REBUILD_SOURCE=1.

step() { printf '\n\033[1;34m== %s ==\033[0m\n' "$1"; }
src()  { PGPASSWORD=pw "$PSQL" "postgresql://app:pw@localhost:${SOURCE_PORT}/tpch" -tAqc "$1" 2>/dev/null; }

ensure_source() {
  [[ -x "$GFS_BIN" ]] || { echo "gfs binary not found at $GFS_BIN  (build: cd $REPO_ROOT && cargo build -p gfs-cli)"; exit 1; }
  command -v duckdb >/dev/null || { echo "duckdb not found on PATH (needed to generate TPC-H)"; exit 1; }

  if ! docker image inspect "$GFS_IMAGE" >/dev/null 2>&1; then
    step "Build the gfs Postgres image ($GFS_IMAGE) — first run only, ~10-20 min"
    docker build -t "$GFS_IMAGE" "$REPO_ROOT/crates/extensions/gfs"
  fi

  if [[ "${DROP_SOURCE:-0}" == 1 ]]; then
    docker rm -f "$SRC_NAME" >/dev/null 2>&1 || true; docker volume rm "$SRC_VOL" >/dev/null 2>&1 || true
    echo "dropped $SRC_NAME + volume $SRC_VOL"; exit 0
  fi
  [[ "${REBUILD_SOURCE:-0}" == 1 ]] && { docker rm -f "$SRC_NAME" >/dev/null 2>&1 || true; docker volume rm "$SRC_VOL" >/dev/null 2>&1 || true; }

  step "TPC-H source SF=$SF (persistent volume $SRC_VOL, tuned)"
  for c in $(docker ps --filter "publish=${SOURCE_PORT}" --format '{{.Names}}' | grep '^gfs-bx-tpch-src-' | grep -vx "$SRC_NAME"); do docker stop "$c" >/dev/null 2>&1 || true; done
  docker rm -f "$SRC_NAME" >/dev/null 2>&1 || true
  docker run -d --name "$SRC_NAME" --shm-size=2g -e POSTGRES_PASSWORD=pw -e POSTGRES_USER=app -e POSTGRES_DB=tpch \
    -v "${SRC_VOL}:/var/lib/postgresql/data" -p "${SOURCE_PORT}:5432" postgres:16 \
    -c shared_buffers=2GB -c work_mem=512MB -c effective_cache_size=6GB \
    -c max_parallel_workers_per_gather=4 -c max_parallel_workers=8 -c jit=off >/dev/null
  echo -n "  waiting for source"
  for i in $(seq 1 90); do docker exec "$SRC_NAME" pg_isready -U app -d tpch >/dev/null 2>&1 && break; echo -n .; sleep 1; done; echo " ok"

  if [[ "$(src 'SELECT count(*) FROM lineitem' 2>/dev/null || echo 0)" -gt 0 ]]; then
    echo "  REUSING existing data (lineitem=$(src 'SELECT count(*) FROM lineitem'), $(src "SELECT pg_size_pretty(pg_database_size('tpch'))"))"
  else
    step "Generate TPC-H (DuckDB) + load with primary keys"
    rm -rf "$DATA_DIR"; mkdir -p "$DATA_DIR"
    duckdb -c "INSTALL tpch; LOAD tpch; CALL dbgen(sf=${SF});
$(for t in region nation supplier part partsupp customer orders lineitem; do echo "COPY ${t} TO '${DATA_DIR}/${t}.csv' (FORMAT csv, HEADER false);"; done)" >/dev/null
    src "
CREATE TABLE region   (r_regionkey int PRIMARY KEY, r_name char(25), r_comment varchar(152));
CREATE TABLE nation   (n_nationkey int PRIMARY KEY, n_name char(25), n_regionkey int, n_comment varchar(152));
CREATE TABLE supplier (s_suppkey int PRIMARY KEY, s_name char(25), s_address varchar(40), s_nationkey int, s_phone char(15), s_acctbal numeric(15,2), s_comment varchar(101));
CREATE TABLE part     (p_partkey int PRIMARY KEY, p_name varchar(55), p_mfgr char(25), p_brand char(10), p_type varchar(25), p_size int, p_container char(10), p_retailprice numeric(15,2), p_comment varchar(23));
CREATE TABLE partsupp (ps_partkey int, ps_suppkey int, ps_availqty int, ps_supplycost numeric(15,2), ps_comment varchar(199), PRIMARY KEY(ps_partkey, ps_suppkey));
CREATE TABLE customer (c_custkey int PRIMARY KEY, c_name varchar(25), c_address varchar(40), c_nationkey int, c_phone char(15), c_acctbal numeric(15,2), c_mktsegment char(10), c_comment varchar(117));
CREATE TABLE orders   (o_orderkey int PRIMARY KEY, o_custkey int, o_orderstatus char(1), o_totalprice numeric(15,2), o_orderdate date, o_orderpriority char(15), o_clerk char(15), o_shippriority int, o_comment varchar(79));
CREATE TABLE lineitem (l_orderkey int, l_partkey int, l_suppkey int, l_linenumber int, l_quantity numeric(15,2), l_extendedprice numeric(15,2), l_discount numeric(15,2), l_tax numeric(15,2), l_returnflag char(1), l_linestatus char(1), l_shipdate date, l_commitdate date, l_receiptdate date, l_shipinstruct char(25), l_shipmode char(10), l_comment varchar(44), PRIMARY KEY(l_orderkey, l_linenumber));" >/dev/null
    for t in region nation supplier part partsupp customer orders lineitem; do
      PGPASSWORD=pw "$PSQL" "postgresql://app:pw@localhost:${SOURCE_PORT}/tpch" -qc "\copy ${t} FROM '${DATA_DIR}/${t}.csv' CSV" >/dev/null
    done
    src "ANALYZE" >/dev/null; rm -rf "$DATA_DIR"
    echo "  loaded: lineitem=$(src 'SELECT count(*) FROM lineitem')  size=$(src "SELECT pg_size_pretty(pg_database_size('tpch'))")"
  fi
}
