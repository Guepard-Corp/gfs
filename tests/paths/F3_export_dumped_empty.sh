#!/usr/bin/env bash
# F3 (#116): a dump reads tables directly, so on a lazy clone every NEVER-READ
# table used to dump as EMPTY. The dump SUCCEEDED, which is what made it
# dangerous: a well-formed backup missing most of the data.
. "$(dirname "$0")/lib/common.sh"
case_begin F3 "exporting a lazy clone produces a COMPLETE dump"
fixture_sql "CREATE TABLE untouched(id int PRIMARY KEY, v text);
             INSERT INTO untouched SELECT g,'u'||g FROM generate_series(1,100) g;"
clone_now
assert_eq "$(local_bytes public.untouched)" "0" "the table has never been read, so it holds nothing locally"
"$BIN" export --format sql >/dev/null 2>&1
F="$WORK/.gfs/exports/export.sql"
if [ -f "$F" ]; then
  N=$(python3 - "$F" <<'PY'
import sys,re
t=open(sys.argv[1],encoding='utf-8',errors='replace').read()
m=re.search(r'COPY public\.untouched\s*\([^)]*\) FROM stdin;\n(.*?)\n\\\.', t, re.S)
print(len([l for l in m.group(1).split('\n') if l.strip()]) if m else 0)
PY
)
  assert_eq "$N" "100" "the never-read table exported all its rows"
else
  no "no export file was produced at $F"
fi
case_end
