"""Judge the lint-scope probe (scripts/check-lint-scopes.sh, ADR-0144).

usage: judge.py <probe lib.rs> <clippy --message-format=json output>

A `// PLANT <lint>` line must draw that lint, by code, on that line; a
`// CONTROL` line must draw nothing; any other diagnostic — an unmarked
line, an error with no lint code — is red: a plant that fails to build
for an unrelated reason is not a pass.
"""
import json
import re
import sys

PLANTS_MIN = 9

src, diag = sys.argv[1], sys.argv[2]
plants, controls = {}, set()
for n, line in enumerate(open(src, encoding="utf-8"), 1):
    m = re.search(r"// PLANT (\S+)\s*$", line)
    if m:
        plants[n] = m.group(1)
    elif re.search(r"// CONTROL\s*$", line):
        controls.add(n)

seen, stray, messages = {}, [], 0
for raw in open(diag, encoding="utf-8"):
    try:
        msg = json.loads(raw)
    except ValueError:
        continue
    if msg.get("reason") != "compiler-message":
        continue
    messages += 1
    d = msg["message"]
    code = (d.get("code") or {}).get("code")
    if code is None:
        text = d["message"]
        if d["level"] == "error" and not text.startswith(("aborting due to", "could not compile")):
            stray.append(text)
        continue
    for sp in d["spans"]:
        if sp.get("is_primary"):
            seen.setdefault(sp["line_start"], set()).add(code)

fail = 0
for ln, lint in sorted(plants.items()):
    if lint not in seen.get(ln, set()):
        drew = sorted(seen.get(ln, []))
        print(f"LINT-SCOPES violation: probe line {ln} did NOT draw {lint} (drew: {drew})")
        fail = 1
for ln in sorted(controls):
    if seen.get(ln):
        print(f"LINT-SCOPES violation: probe control line {ln} drew {sorted(seen[ln])}")
        fail = 1
for ln, codes in sorted(seen.items()):
    if ln not in plants and ln not in controls:
        print(f"LINT-SCOPES violation: probe line {ln} drew {sorted(codes)} with no PLANT marker")
        fail = 1
for text in stray:
    print(f"LINT-SCOPES violation: the probe failed for an unrelated reason: {text}")
    fail = 1
if len(plants) < PLANTS_MIN:
    print(f"LINT-SCOPES SCOPE ERROR: {len(plants)} plants (expected ≥ {PLANTS_MIN}) — edited down")
    fail = 1
if messages == 0:
    print("LINT-SCOPES SCOPE ERROR: clippy produced no diagnostic at all — the probe did not run")
    fail = 1
if fail:
    sys.exit(1)
print(f"lint-scopes probe OK: {len(plants)} plants each drew its lint by name, "
      f"{len(controls)} controls clean")
