#!/usr/bin/env bash
# Master runner for the per-path tests.
#
#   ./run-all.sh              run every implemented path
#   ./run-all.sh B            run the B family
#   ./run-all.sh B4 D3        run specific paths
#   ./run-all.sh --list       show coverage against the document, run nothing
#
# Exit code is the number of FAILING paths. An ABORT (environment/setup failure)
# is reported separately and never counted as a product defect: this project has
# repeatedly produced runs where one bad clone printed a dozen convincing
# failures that meant nothing.
set -uo pipefail
cd "$(dirname "$0")" || exit 1

# Every case ID the document declares, so coverage is measured against the
# document rather than against whatever scripts happen to exist.
DOC_CASES="A1 A2 A3 A4 A5 \
B1 B2 B3 B4 B4b B5 B6 B7 B8 B9 B10 B10b B11 B12 B13 B14 B15 B16 B16b B17 B18 B19 \
C1 C2 C3 C4 C5 C6 C7 C8 \
D1 D2 D3 D4 D5 D6 D7 D8 D9 \
E1 E1b E2 E2b F1 F1b F1c F2 F3 G1 G2 \
H1 H2 H3 H4 H5 H6 H7 H8"

# Each case lives in its own folder: <CASE>_<slug>/test.sh, beside its README.
script_for(){ ls -d "$1"_*/test.sh 2>/dev/null | head -1; }

if [ "${1:-}" = "--list" ]; then
  printf "%-6s %-10s %s\n" CASE STATUS SCRIPT
  have=0; miss=0
  for c in $DOC_CASES; do
    s=$(script_for "$c")
    if [ -n "$s" ]; then printf "%-6s %-10s %s\n" "$c" "covered" "$(dirname "$s")/"; have=$((have+1))
    else                printf "%-6s %-10s %s\n" "$c" "MISSING" "-";   miss=$((miss+1)); fi
  done
  echo
  echo "$have of $((have+miss)) documented paths have a script; $miss missing."
  exit 0
fi

# Which scripts to run
SEL=()
if [ $# -eq 0 ]; then
  while IFS= read -r f; do SEL+=("$f"); done < <(ls -d [A-H]*_*/test.sh 2>/dev/null | sort -V)
else
  # Match a single LETTER as a family (B -> every B case), anything else as an
  # EXACT case id. Prefix matching would make "B1" also run B10, B11, B12 ... B19,
  # which silently turns a one-test check into a twelve-test sweep.
  for a in "$@"; do
    if [[ "$a" =~ ^[A-H]$ ]]; then
      pat="$a[0-9]*_*/test.sh"
    else
      pat="${a}_*/test.sh"
    fi
    while IFS= read -r f; do [ -n "$f" ] && SEL+=("$f"); done < <(ls -d $pat 2>/dev/null | sort -V)
  done
fi
[ ${#SEL[@]} -eq 0 ] && { echo "no matching path scripts"; exit 1; }

# Free the disk before a long sweep: this VM is small and a starved clone is the
# single most common cause of a bogus failure here.
docker ps -a --filter status=exited --format '{{.Names}}' | grep -E '^gfs-postgres-[0-9]+$' | xargs -r docker rm -f >/dev/null 2>&1
docker volume prune -f >/dev/null 2>&1

PASSED=(); FAILED=(); ABORTED=(); OPENFIXED=()
START=$(date +%s)
for s in "${SEL[@]}"; do
  bash "$s"
  rc=$?
  case $rc in
    0) PASSED+=("$(dirname "$s")") ;;
    91) OPENFIXED+=("$(dirname "$s")") ;;
    90) ABORTED+=("$(dirname "$s")") ;;
    *) FAILED+=("$(dirname "$s")") ;;
  esac
  # keep the VM from filling mid-sweep
  docker ps -a --filter status=exited --format '{{.Names}}' | grep -E '^gfs-postgres-[0-9]+$' | xargs -r docker rm -f >/dev/null 2>&1
  docker volume prune -f >/dev/null 2>&1
done
ELAPSED=$(( $(date +%s) - START ))

echo
echo "================================ SUMMARY ================================"
echo "ran ${#SEL[@]} path(s) in ${ELAPSED}s"
echo "  passed : ${#PASSED[@]}"
echo "  failed : ${#FAILED[@]}"
[ ${#FAILED[@]}    -gt 0 ] && printf '           %s\n' "${FAILED[@]}"
echo "  aborted: ${#ABORTED[@]}   (environment/setup, NOT a product defect)"
[ ${#ABORTED[@]}   -gt 0 ] && printf '           %s\n' "${ABORTED[@]}"
if [ ${#OPENFIXED[@]} -gt 0 ]; then
  echo "  known-open paths that now PASS (update the document): ${#OPENFIXED[@]}"
  printf '           %s\n' "${OPENFIXED[@]}"
fi

# Coverage, so a green run never reads as "everything is proven"
have=0; miss=""
for c in $DOC_CASES; do
  if [ -n "$(script_for "$c")" ]; then have=$((have+1)); else miss="$miss $c"; fi
done
total=$(printf '%s\n' $DOC_CASES | wc -w | tr -d ' ')
echo "  coverage: $have of $total documented paths have a script"
[ -n "$miss" ] && echo "  no script yet:$miss"
echo "========================================================================="
exit ${#FAILED[@]}
