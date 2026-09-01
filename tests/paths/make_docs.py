#!/usr/bin/env python3
"""Regenerate the per-test README files from the test scripts themselves.

Run from tests/paths:   python3 make_docs.py

Everything in a per-test README is DERIVED from its test.sh: the description, the
issue numbers, the fixture, what the source does, and what is asserted. That is
deliberate. A hand-written description drifts from the test the first time someone
edits the script; a generated one cannot.

If you change a test, re-run this and commit the README with it.
"""

import os
import re
import glob

# ---------------------------------------------------------------- fixtures
FIXTURES = {
    "fixture_simple": (
        "orders(id, customer, total) with 3 rows: Alice 50, Bob 30, Carol 20\n"
        "notes(id, body) with 1 row\n"
        "other(id, v) with 1 row, used only to trigger background checks"
    ),
    "fixture_bulk": (
        "orders(id, customer, total) with 5000 rows, big enough that a whole copy\n"
        "is visible in the clone's own statistics\n"
        "other(id, v) with 1 row"
    ),
    "fixture_sql": "a custom schema, created inline by this test (see the script)",
}

HELPERS = {
    "src": "runs SQL on the SOURCE database",
    "val": "reads through the gfs CLI and returns one value",
    "q": "reads through the gfs CLI and returns the whole output",
    "P": "runs SQL directly inside the clone container",
    "nudge": "reads an unrelated table so a background drift check can run and commit",
    "clone_now": "clones the source and waits until the clone is queryable",
    "wait_until_cached": "polls until the table is genuinely held whole locally",
    "with_source_down": "evaluates a query with the source stopped, then restarts it",
    "local_bytes": "physical size of the local heap; never scans, so the planner hook cannot fire",
    "clone_state": "reads a column of gfs.clone_source for one table",
    "assert_source_untouched": "asserts the source's write counters never moved",
}


def parse(path):
    src = open(path).read()
    out = {"path": path, "src": src}

    m = re.search(r'^case_begin\s+(\S+)\s+"([^"]*)"(.*)$', src, re.M)
    out["case"] = m.group(1) if m else "?"
    out["desc"] = m.group(2) if m else ""
    out["open"] = bool(m and "--expect open" in m.group(3))

    # the leading comment block is the author's rationale
    lines, note = src.splitlines(), []
    for ln in lines[1:]:
        if ln.startswith("#"):
            note.append(ln.lstrip("#").strip())
        elif ln.strip() == "":
            if note:
                break
        else:
            break
    out["note"] = "\n".join(note).strip()

    out["issues"] = sorted(set(re.findall(r"#(\d{3})", src)), key=int)
    out["fixture"] = next((f for f in FIXTURES if f in src), None)
    out["src_actions"] = re.findall(r'^\s*src\s+"([^"]+)"', src, re.M)
    out["asserts"] = [a for a in re.findall(r'"([^"]{12,})"\s*$', src, re.M)]
    # assertion messages are the LAST string argument on an assert_* line
    out["asserts"] = re.findall(r'assert_\w+\s+.*?"([^"]+)"\s*$', src, re.M)
    out["oks"] = re.findall(r'^\s*(?:\|\|\s*)?(?:ok|no)\s+"([^"]+)"', src, re.M)
    out["helpers"] = sorted({h for h in HELPERS if re.search(rf"\b{h}\b", src)})
    return out


def render(d):
    case, desc = d["case"], d["desc"]
    L = []
    L.append(f"# {case}: {desc}")
    L.append("")
    if d["open"]:
        L.append("> **This test is expected to FAIL.** It is marked `--expect open`, which means it")
        L.append("> asserts what the system does *today* for something that is still unfixed. The")
        L.append("> suite counts it as passing. If it ever starts passing on its own, the runner")
        L.append("> exits 3 and tells you the documentation is out of date.")
        L.append("")
    if d["issues"]:
        links = ", ".join(
            f"[#{i}](https://github.com/Guepard-Corp/gfs/issues/{i})" for i in d["issues"]
        )
        L.append(f"**Related issues:** {links}")
        L.append("")

    if d["note"]:
        L.append("## Why this test exists")
        L.append("")
        L.append(d["note"])
        L.append("")

    L.append("## The scenario")
    L.append("")
    if d["fixture"]:
        L.append("**The source starts with:**")
        L.append("")
        L.append("```")
        L.append(FIXTURES[d["fixture"]])
        L.append("```")
        L.append("")
    if d["src_actions"]:
        L.append("**Then the source does:**")
        L.append("")
        L.append("```sql")
        for a in d["src_actions"]:
            L.append(a.strip())
        L.append("```")
        L.append("")
    else:
        L.append("The source is not modified: this test is about what the clone does on its own.")
        L.append("")

    if d["asserts"] or d["oks"]:
        L.append("## What is asserted")
        L.append("")
        for a in d["asserts"] + d["oks"]:
            L.append(f"- {a}")
        L.append("")

    L.append("## Running it")
    L.append("")
    L.append("```bash")
    L.append(f"tests/paths/run-all.sh {case}          # through the runner")
    L.append(f"bash tests/paths/{os.path.basename(os.path.dirname(d['path']))}/test.sh   # directly")
    L.append("```")
    L.append("")
    L.append("Each test builds its **own** throwaway source and its **own** clone, so it can be")
    L.append("run alone and cannot be affected by any other test.")
    L.append("")

    if d["helpers"]:
        L.append("## Harness helpers used")
        L.append("")
        L.append("| helper | what it does |")
        L.append("| --- | --- |")
        for h in d["helpers"]:
            L.append(f"| `{h}` | {HELPERS[h]} |")
        L.append("")

    L.append("See [`../lib/common.sh`](../lib/common.sh) for the harness, and")
    L.append("[`../README.md`](../README.md) for the traps that have produced false results here.")
    L.append("")
    return "\n".join(L)


def main():
    made = 0
    for path in sorted(glob.glob("*/test.sh")):
        d = parse(path)
        readme = os.path.join(os.path.dirname(path), "README.md")
        open(readme, "w").write(render(d))
        made += 1
    print(f"  wrote {made} per-test README file(s)")


if __name__ == "__main__":
    main()
