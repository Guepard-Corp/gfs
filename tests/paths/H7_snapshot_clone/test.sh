#!/usr/bin/env bash
# H7 (#132): `gfs clone --snapshot` = clone + freeze in one step. The result is
# born detached: consistent, immune to source writes, and answers offline.
. "$(dirname "$0")/../lib/common.sh"
case_begin H7 "clone --snapshot yields a detached point-in-time clone"
fixture_simple
mkdir -p "$WORK" || abort "cannot create $WORK"
"$BIN" clone --from "postgres://postgres:x@host.docker.internal:$PORT/postgres" \
      --image "$IMAGE" --snapshot "$WORK" > "$WORK/clone.log" 2>&1
RC=$?
cd "$WORK" || abort "cannot enter $WORK"
CID=$(grep container_name .gfs/config.toml 2>/dev/null | sed 's/.*= *"//;s/".*//')
[ -z "$CID" ] && { echo "    clone log:"; tail -8 "$WORK/clone.log" | sed 's/^/      /'; abort "clone did not complete"; }
for i in $(seq 1 45); do docker exec "$CID" psql -U postgres -qtAc "SELECT 1" >/dev/null 2>&1 && break; sleep 1; done

[ "$RC" -eq 0 ] && ok "clone --snapshot exited 0" \
                || { no "clone --snapshot failed (rc=$RC)"; tail -8 "$WORK/clone.log" | sed 's/^/      /'; }
assert_match "$(clone_log)" "Snapshot clone ready" "the CLI announces the snapshot"
assert_true "$(P "SELECT frozen::text FROM gfs.clone_mode;")" "the clone is born frozen"
assert_eq "$(P "SELECT count(*) FROM gfs.clone_source WHERE NOT whole_cached;")" "0" "every table was copied up front"

src "INSERT INTO orders VALUES (4,'Dave',40);"
nudge
assert_query_eq "SELECT count(*) FROM orders;" 3 "source writes after the snapshot are invisible"
V=$(with_source_down "SELECT count(*) FROM orders;")
assert_eq "$V" "3" "and the snapshot answers with the source stopped"
case_end
