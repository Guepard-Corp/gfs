#!/usr/bin/env bash
# PoC: "faithful schema + side-schema overlay, proxy-routed".
#
# Goal under test: keep the in-DB copy-on-read overlay, but stop letting the
# overlay VIEW occupy the table's real name. Instead:
#
#   <schema>.<table>          = the 100% FAITHFUL real table (= local store) with
#                               the source's triggers / indexes / constraints.
#   gfs_ovl__<schema>.<table> = the federation VIEW (local UNION ALL foreign
#                               minus local minus deleted) + INSTEAD OF triggers.
#   gfs_remote_<schema>       = foreign tables (postgres_fdw).
#   gfs_sync                  = tombstones / metadata.
#
# The "proxy" is simulated by INTERLEAVING the session search_path:
#     gfs_ovl__shop, shop, gfs_ovl__audit, audit
# so an UNQUALIFIED read resolves to the overlay (federated) while writes,
# triggers and functions operate on the real faithful tables.
#
# What we measure:
#   T1  faithful reads federate via interleaved search_path (real table empty)
#   T2  copy-on-write through the overlay lands in the faithful table AND fires
#       the source's BEFORE + AFTER triggers (the key win: views couldn't do this)
#   T3  RESIDUAL: a source FUNCTION (SET search_path=shop) bypasses the overlay
#       and sees only local rows -> undercount
#   T4  RESIDUAL: a SCHEMA-QUALIFIED read bypasses the overlay too
#   T5  CONVERGENCE: warming the table whole makes the faithful table complete,
#       so T3/T4 become correct -> warming is the single lever that closes the gap
#
# Requires Docker. Self-contained; cleans up on exit.
set -euo pipefail

NET=gfs-poc-faithful-net
REMOTE=gfs-poc-faithful-remote
LOCAL=gfs-poc-faithful-local
IMG=postgres:16

cleanup() {
  docker rm -f "$REMOTE" "$LOCAL" >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

rpsql() { docker exec -i "$REMOTE" psql -U postgres -d postgres -v ON_ERROR_STOP=1 -tAc "$1"; }
lpsql() { docker exec -i "$LOCAL"  psql -U postgres -d postgres -v ON_ERROR_STOP=1 -tAc "$1"; }

wait_ready() {
  local c=$1 deadline=$((SECONDS+60))
  until docker exec "$c" pg_isready -U postgres -d postgres >/dev/null 2>&1; do
    [ $SECONDS -lt $deadline ] || { echo "FAIL: $c never ready"; exit 1; }
    sleep 0.5
  done
}

echo "== Starting two postgres on a shared network =="
docker network create "$NET" >/dev/null
docker run -d --name "$REMOTE" --network "$NET" -e POSTGRES_PASSWORD=postgres "$IMG" >/dev/null
docker run -d --name "$LOCAL"  --network "$NET" -e POSTGRES_PASSWORD=postgres "$IMG" >/dev/null
wait_ready "$REMOTE"; wait_ready "$LOCAL"

# ---------------------------------------------------------------------------
echo "== Seed SOURCE: two schemas (shop, audit), a BEFORE+AFTER trigger, a fn =="
# ---------------------------------------------------------------------------
docker exec -i "$REMOTE" psql -U postgres -d postgres -v ON_ERROR_STOP=1 >/dev/null <<'SQL'
CREATE SCHEMA shop;
CREATE SCHEMA audit;

CREATE TABLE audit.product_log (
  id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  product_id bigint NOT NULL,
  action     text   NOT NULL,
  at         timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE shop.products (
  id          bigint PRIMARY KEY,
  name        text NOT NULL,
  price_cents int  NOT NULL,
  updated_at  timestamptz
);

-- BEFORE: normalize (uppercase name, stamp updated_at).  AFTER: cross-schema audit.
CREATE FUNCTION shop.tg_products_before() RETURNS trigger LANGUAGE plpgsql AS $b$
BEGIN
  NEW.name := upper(NEW.name);
  NEW.updated_at := now();
  RETURN NEW;
END $b$;
CREATE FUNCTION shop.tg_products_after() RETURNS trigger LANGUAGE plpgsql AS $b$
BEGIN
  INSERT INTO audit.product_log(product_id, action) VALUES (NEW.id, TG_OP);
  RETURN NEW;
END $b$;
CREATE TRIGGER products_before BEFORE INSERT OR UPDATE ON shop.products
  FOR EACH ROW EXECUTE FUNCTION shop.tg_products_before();
CREATE TRIGGER products_after  AFTER  INSERT OR UPDATE ON shop.products
  FOR EACH ROW EXECUTE FUNCTION shop.tg_products_after();

-- Completeness-dependent function (resolves names against ITS OWN search_path).
CREATE FUNCTION shop.count_products() RETURNS bigint
  LANGUAGE sql STABLE SET search_path = shop AS 'SELECT count(*) FROM products';

INSERT INTO shop.products(id, name, price_cents)
SELECT g, 'gizmo-'||g, 100*g FROM generate_series(1,5) g;
SQL
echo "   source shop.products = $(rpsql "SELECT count(*) FROM shop.products"), names = $(rpsql "SELECT string_agg(name,',' ORDER BY id) FROM shop.products")"

# ---------------------------------------------------------------------------
echo "== Build CLONE: faithful tables + FDW + per-schema overlays + INSTEAD OF =="
# ---------------------------------------------------------------------------
cat <<'SQL' | sed "s/__REMOTE__/$REMOTE/g" | docker exec -i "$LOCAL" psql -U postgres -d postgres -v ON_ERROR_STOP=1 >/dev/null
CREATE EXTENSION IF NOT EXISTS postgres_fdw;
CREATE SERVER gfs_remote_srv FOREIGN DATA WRAPPER postgres_fdw
  OPTIONS (host '__REMOTE__', port '5432', dbname 'postgres');
CREATE USER MAPPING FOR CURRENT_USER SERVER gfs_remote_srv
  OPTIONS (user 'postgres', password 'postgres');
CREATE SCHEMA gfs_sync;

-- (1) FAITHFUL schema: real tables + the source's triggers/functions, verbatim.
CREATE SCHEMA shop;
CREATE SCHEMA audit;
CREATE TABLE audit.product_log (
  id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  product_id bigint NOT NULL,
  action     text   NOT NULL,
  at         timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE shop.products (
  id          bigint PRIMARY KEY,
  name        text NOT NULL,
  price_cents int  NOT NULL,
  updated_at  timestamptz
);
CREATE FUNCTION shop.tg_products_before() RETURNS trigger LANGUAGE plpgsql AS $b$
BEGIN
  NEW.name := upper(NEW.name);
  NEW.updated_at := now();
  RETURN NEW;
END $b$;
CREATE FUNCTION shop.tg_products_after() RETURNS trigger LANGUAGE plpgsql AS $b$
BEGIN
  INSERT INTO audit.product_log(product_id, action) VALUES (NEW.id, TG_OP);
  RETURN NEW;
END $b$;
CREATE TRIGGER products_before BEFORE INSERT OR UPDATE ON shop.products
  FOR EACH ROW EXECUTE FUNCTION shop.tg_products_before();
CREATE TRIGGER products_after  AFTER  INSERT OR UPDATE ON shop.products
  FOR EACH ROW EXECUTE FUNCTION shop.tg_products_after();
CREATE FUNCTION shop.count_products() RETURNS bigint
  LANGUAGE sql STABLE SET search_path = shop AS 'SELECT count(*) FROM products';

-- (2) Foreign tables (only what we federate: shop.products).
CREATE SCHEMA gfs_remote_shop;
IMPORT FOREIGN SCHEMA shop LIMIT TO (products) FROM SERVER gfs_remote_srv INTO gfs_remote_shop;

-- (3) Tombstones for copy-on-write deletes.
CREATE TABLE gfs_sync.shop_products_deleted (k text PRIMARY KEY);

-- (4) Side-schema overlay VIEW (reserved prefix) + INSTEAD OF copy-on-write.
CREATE SCHEMA gfs_ovl__shop;
CREATE VIEW gfs_ovl__shop.products AS
  SELECT id, name, price_cents, updated_at FROM shop.products
  UNION ALL
  SELECT r.id, r.name, r.price_cents, r.updated_at FROM gfs_remote_shop.products r
   WHERE NOT EXISTS (SELECT 1 FROM shop.products l            WHERE l.id = r.id)
     AND NOT EXISTS (SELECT 1 FROM gfs_sync.shop_products_deleted d WHERE d.k = r.id::text);

-- INSTEAD OF INSERT: write the faithful table (fires its BEFORE/AFTER triggers).
CREATE FUNCTION gfs_ovl__shop.tg_products_ins() RETURNS trigger LANGUAGE plpgsql AS $b$
BEGIN
  INSERT INTO shop.products(id, name, price_cents, updated_at)
       VALUES (NEW.id, NEW.name, NEW.price_cents, NEW.updated_at)
  ON CONFLICT (id) DO NOTHING;
  DELETE FROM gfs_sync.shop_products_deleted WHERE k = NEW.id::text;
  RETURN NEW;
END $b$;
-- INSTEAD OF UPDATE: copy-on-write — hydrate the remote row, then UPDATE it
-- (the UPDATE fires the faithful triggers).
CREATE FUNCTION gfs_ovl__shop.tg_products_upd() RETURNS trigger LANGUAGE plpgsql AS $b$
BEGIN
  INSERT INTO shop.products(id, name, price_cents, updated_at)
       SELECT id, name, price_cents, updated_at FROM gfs_remote_shop.products WHERE id = OLD.id
  ON CONFLICT (id) DO NOTHING;
  UPDATE shop.products
     SET name = NEW.name, price_cents = NEW.price_cents
   WHERE id = OLD.id;
  RETURN NEW;
END $b$;
-- INSTEAD OF DELETE: drop locally + tombstone the key.
CREATE FUNCTION gfs_ovl__shop.tg_products_del() RETURNS trigger LANGUAGE plpgsql AS $b$
BEGIN
  DELETE FROM shop.products WHERE id = OLD.id;
  INSERT INTO gfs_sync.shop_products_deleted(k) VALUES (OLD.id::text) ON CONFLICT DO NOTHING;
  RETURN OLD;
END $b$;
CREATE TRIGGER ovl_ins INSTEAD OF INSERT ON gfs_ovl__shop.products FOR EACH ROW EXECUTE FUNCTION gfs_ovl__shop.tg_products_ins();
CREATE TRIGGER ovl_upd INSTEAD OF UPDATE ON gfs_ovl__shop.products FOR EACH ROW EXECUTE FUNCTION gfs_ovl__shop.tg_products_upd();
CREATE TRIGGER ovl_del INSTEAD OF DELETE ON gfs_ovl__shop.products FOR EACH ROW EXECUTE FUNCTION gfs_ovl__shop.tg_products_del();
SQL

# "proxy" routing: interleave overlay schema before each source schema.
SP="SET search_path TO gfs_ovl__shop, shop, audit;"
# tail -1 drops the "SET" command tag that psql prints before the SELECT result.
ovl()  { lpsql "$SP SELECT count(*) FROM products;" | tail -1; }  # unqualified -> overlay
qual() { lpsql "SELECT count(*) FROM shop.products;"; }           # qualified  -> faithful (partial)
faith(){ lpsql "SELECT count(*) FROM shop.products;"; }
fn()   { lpsql "SELECT shop.count_products();"; }                 # function (own search_path)

echo
echo "================ T1: faithful reads federate via interleaved search_path ========"
OVL1=$(ovl); FAITH1=$(faith)
echo "   overlay (unqualified) count = $OVL1   |   faithful shop.products = $FAITH1"

echo
echo "================ T2: copy-on-write via overlay fires the faithful triggers ======"
# INSERT a brand-new product through the OVERLAY (lower-case name on purpose).
lpsql "$SP INSERT INTO products(id, name, price_cents) VALUES (100, 'new-widget', 500);" >/dev/null
# UPDATE a FEDERATED (not-yet-local) product through the overlay -> copy-on-write.
lpsql "$SP UPDATE products SET price_cents = 999 WHERE id = 2;" >/dev/null
NAME100=$(lpsql "SELECT name FROM shop.products WHERE id = 100;")
PRICE2=$(lpsql "SELECT price_cents FROM shop.products WHERE id = 2;")
NAME2=$(lpsql "SELECT name FROM shop.products WHERE id = 2;")
AUDIT100=$(lpsql "SELECT count(*) FROM audit.product_log WHERE product_id = 100;")
AUDIT2=$(lpsql "SELECT count(*) FROM audit.product_log WHERE product_id = 2;")
OVL2=$(ovl)
echo "   id=100 name='$NAME100' (BEFORE upper) | id=2 price=$PRICE2 name='$NAME2' (copy-on-write+upper)"
echo "   audit rows: id100=$AUDIT100  id2=$AUDIT2 (AFTER trigger) | overlay count now = $OVL2"

echo
echo "================ T3 + T4: RESIDUAL — fn & qualified read bypass the overlay ======"
FN3=$(fn); QUAL4=$(qual)
echo "   shop.count_products() = $FN3   |   SELECT FROM shop.products (qualified) = $QUAL4   |   overlay = $OVL2"

echo
echo "================ T5: CONVERGENCE — warm the table whole, residual closes ========="
# Warming hydrates the rest into the faithful table. Suppress triggers during the
# bulk copy (session_replication_role=replica) — a real design constraint the PoC
# surfaces: hydration must NOT fire business triggers.
lpsql "SET session_replication_role = replica;
       INSERT INTO shop.products(id,name,price_cents,updated_at)
         SELECT id,name,price_cents,updated_at FROM gfs_remote_shop.products r
          WHERE NOT EXISTS (SELECT 1 FROM shop.products l WHERE l.id=r.id)
            AND NOT EXISTS (SELECT 1 FROM gfs_sync.shop_products_deleted d WHERE d.k=r.id::text);" >/dev/null
FN5=$(fn); QUAL5=$(qual); OVL5=$(ovl)
echo "   after warm:  shop.count_products() = $FN5   |   qualified = $QUAL5   |   overlay = $OVL5"

# ---------------------------------------------------------------------------
echo
echo "================================ RESULTS ========================================"
fail=0
chk() { if [ "$2" = "1" ]; then echo "  PASS  $1"; else echo "  FAIL  $1"; fail=1; fi; }

chk "T1: overlay sees all 5 source rows while faithful table is empty [$OVL1/$FAITH1]" \
    "$([ "$OVL1" = "5" ] && [ "$FAITH1" = "0" ] && echo 1 || echo 0)"

chk "T2: INSERT via overlay -> faithful table, BEFORE trigger upper-cased [$NAME100]" \
    "$([ "$NAME100" = "NEW-WIDGET" ] && echo 1 || echo 0)"
chk "T2: UPDATE via overlay copy-on-write of federated id=2 [price=$PRICE2 name=$NAME2]" \
    "$([ "$PRICE2" = "999" ] && [ "$NAME2" = "GIZMO-2" ] && echo 1 || echo 0)"
chk "T2: AFTER trigger fired (audit rows for id100 & id2) [$AUDIT100/$AUDIT2]" \
    "$([ "$AUDIT100" -ge 1 ] && [ "$AUDIT2" -ge 1 ] && echo 1 || echo 0)"
chk "T2: overlay count = 6 (5 remote + new id=100) [$OVL2]" \
    "$([ "$OVL2" = "6" ] && echo 1 || echo 0)"

chk "T3: RESIDUAL confirmed — fn (own search_path) undercounts vs overlay [$FN3 < $OVL2]" \
    "$([ "$FN3" -lt "$OVL2" ] && echo 1 || echo 0)"
chk "T4: RESIDUAL confirmed — qualified read undercounts vs overlay [$QUAL4 < $OVL2]" \
    "$([ "$QUAL4" -lt "$OVL2" ] && echo 1 || echo 0)"

chk "T5: CONVERGENCE — fn, qualified and overlay all agree after warm [$FN5/$QUAL5/$OVL5]" \
    "$([ "$FN5" = "$OVL5" ] && [ "$QUAL5" = "$OVL5" ] && [ "$OVL5" = "6" ] && echo 1 || echo 0)"

echo
if [ "$fail" = "0" ]; then
  echo "ALL PASS — faithful tables keep the real name (triggers/functions/indexes work),"
  echo "the side-schema overlay federates reads and does copy-on-write into the real"
  echo "table (firing its triggers), the residual (qualified/function-internal reads) is"
  echo "real and measurable, and warming the table whole closes it."
else
  echo "SOME CHECKS FAILED — see above."
  exit 1
fi
