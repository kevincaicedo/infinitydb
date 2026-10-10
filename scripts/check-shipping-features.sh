#!/usr/bin/env bash
# Shipping-feature fence (ADR-0107; review of 2026-08-30, F-L16-01 / P4).
#
# `inf-foundation`'s `fault-points` and `collision-oracle` features are
# test/DST machinery: a live fault registry on every durability step, and
# a hasher under which any two 48-byte `{shadow-collide}` keys collide — a
# remote hash-flooding primitive if it ever ships. Cargo unifies the
# features of NORMAL dependency edges across every package built in one
# invocation, so one workspace member requesting either feature on a
# normal edge turns it on for `infinityd` in `cargo build --workspace`
# (the documented developer build) — which is exactly what `inf-sim`'s
# manifest did until this gate existed. "OFF in every shipping build" was
# true only because three release commands happened to say `-p infinityd`.
#
# Two checks, both asserting their scope:
#   1. Manifest grammar (no cargo; self-testable through INF_CHECK_ROOT):
#      every Cargo.toml under crates/, bins/ and tests/ is scanned; a
#      banned feature may appear only on a `[dev-dependencies]` edge or
#      inside a `[features]` forwarder (`x = ["inf-foundation/fault-points"]`)
#      that `default` does not reach. A normal edge, a `[target.*.dependencies]`
#      edge, or a `default` that reaches a forwarder is a violation. Zero
#      manifests is a scope failure.
#   2. Resolver truth (skipped only when cargo is absent or a fixture root
#      is under test): for each fenced package, `cargo tree --workspace -e
#      features,normal -i <package>` must list none of its banned
#      features, and the two shipping packages resolved alone must not
#      either.
#
# `inf-store`'s `test-support` (ADR-0139 D2) is fenced the same way: it
# exports constructors that forge what only the engine may build (an
# `AliasView` with no enumeration behind it), for bare-tree suites.
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "${INF_CHECK_ROOT:-$SCRIPT_DIR/..}"

BANNED='collision-oracle|fault-points|test-support'
# Resolver rows: `<package>:<its banned features>`.
FENCED=('inf-foundation:collision-oracle|fault-points' 'inf-store:collision-oracle|test-support')
fail=0

manifests=()
for dir in crates bins tests; do
    [ -d "$dir" ] || continue
    while IFS= read -r f; do manifests+=("$f"); done < <(find "$dir" -name Cargo.toml -not -path '*/target/*' | sort)
done
if [ "${#manifests[@]}" -eq 0 ]; then
    echo "SHIPPING-FEATURE SCOPE ERROR: no Cargo.toml under crates/, bins/ or tests/ from $(pwd)"
    exit 1
fi

forwarders=0
for f in "${manifests[@]}"; do
    # One pass per manifest. Sections: normal dependency tables (including
    # `[dependencies.NAME]` and `[target.'cfg'.dependencies]` forms) are
    # "normal"; dev/build tables are exempt; `[features]` is collected so
    # `default`'s closure can be checked against the forwarders.
    out=$(awk -v banned="$BANNED" '
        function trim(s) { sub(/^[[:space:]]+/, "", s); sub(/[[:space:]]+$/, "", s); return s }
        function is_banned_feature_list(s) {
            # `"fault-points"`, `"inf-foundation/fault-points"`, `"inf-foundation?/fault-points"`
            return s ~ ("\"([a-z-]+\\??/)?(" banned ")\"")
        }
        BEGIN { section = ""; feat = ""; collecting = 0 }
        /^[[:space:]]*#/ { next }
        /^[[:space:]]*\[/ {
            line = $0; sub(/^[[:space:]]*\[/, "", line); sub(/\].*$/, "", line)
            if (line ~ /^(target\.[^.]+\.)?dependencies(\.|$)/) section = "normal"
            else if (line ~ /^(target\.[^.]+\.)?(dev|build)-dependencies(\.|$)/) section = "exempt"
            else if (line == "features") section = "features"
            else section = "other"
            collecting = 0
            next
        }
        section == "normal" {
            if ($0 ~ /features[[:space:]]*=/ && is_banned_feature_list($0)) {
                print "V " NR ": " trim($0)
            }
            next
        }
        section == "features" {
            if (collecting) {
                acc = acc " " $0
                if ($0 ~ /\]/) { defs[feat] = acc; collecting = 0 }
                next
            }
            if (match($0, /^[[:space:]]*[A-Za-z0-9_-]+[[:space:]]*=/)) {
                feat = substr($0, RSTART, RLENGTH); sub(/[[:space:]]*=$/, "", feat); feat = trim(feat)
                acc = $0
                if ($0 ~ /\]/) { defs[feat] = acc } else { collecting = 1 }
            }
            next
        }
        END {
            for (name in defs) if (is_banned_feature_list(defs[name])) { fwd[name] = 1; nfwd++ }
            # `default` closure over the feature graph.
            if ("default" in defs) {
                queue[1] = "default"; qn = 1; qi = 1; seen["default"] = 1
                while (qi <= qn) {
                    cur = queue[qi++]
                    if (cur in fwd) print "D default reaches forwarder " cur
                    def = defs[cur]
                    while (match(def, /"[A-Za-z0-9_-]+"/)) {
                        dep = substr(def, RSTART + 1, RLENGTH - 2)
                        def = substr(def, RSTART + RLENGTH)
                        if ((dep in defs) && !(dep in seen)) { seen[dep] = 1; queue[++qn] = dep }
                    }
                }
            }
            print "F " nfwd + 0
        }' "$f")
    while IFS= read -r row; do
        case "$row" in
            V\ *) echo "SHIPPING-FEATURE violation: $f:${row#V } — a normal dependency edge requests a test/DST feature (ADR-0107: dev-dependencies or an explicit non-default feature only)"; fail=1 ;;
            D\ *) echo "SHIPPING-FEATURE violation: $f — ${row#D } (a default feature must never reach a test/DST feature)"; fail=1 ;;
            F\ *) forwarders=$((forwarders + ${row#F })) ;;
        esac
    done <<< "$out"
done

resolver="not run (fixture root or no cargo)"
if [ -z "${INF_CHECK_ROOT:-}" ] && command -v cargo >/dev/null 2>&1; then
    resolver=""
    for fence in "${FENCED[@]}"; do
        fenced_pkg=${fence%%:*}
        fenced_features=${fence#*:}
        tree=$(cargo tree --workspace -e features,normal -i "$fenced_pkg" 2>&1) || {
            echo "SHIPPING-FEATURE SCOPE ERROR: cargo tree -i $fenced_pkg failed:"
            echo "$tree" | sed 's/^/    /'
            exit 1
        }
        if ! printf '%s\n' "$tree" | grep -q "^$fenced_pkg v"; then
            echo "SHIPPING-FEATURE SCOPE ERROR: cargo tree did not resolve $fenced_pkg"
            exit 1
        fi
        if printf '%s\n' "$tree" | grep -E "$fenced_pkg feature \"($fenced_features)\"" >/dev/null; then
            echo "SHIPPING-FEATURE violation: the workspace's normal-edge graph enables a banned $fenced_pkg feature:"
            printf '%s\n' "$tree" | grep -E -A2 "$fenced_pkg feature \"($fenced_features)\"" | sed 's/^/    /'
            fail=1
        fi
        for pkg in infinityd inf; do
            one=$(cargo tree -p "$pkg" -e features,normal -i "$fenced_pkg" 2>&1) || {
                # A shipping package that does not depend on the fenced
                # one at all cannot enable its features.
                if printf '%s\n' "$one" | grep -q "did not match any packages"; then continue; fi
                echo "SHIPPING-FEATURE SCOPE ERROR: cargo tree -p $pkg -i $fenced_pkg failed:"
                echo "$one" | sed 's/^/    /'
                exit 1
            }
            if printf '%s\n' "$one" | grep -E "$fenced_pkg feature \"($fenced_features)\"" >/dev/null; then
                echo "SHIPPING-FEATURE violation: $pkg alone resolves a banned $fenced_pkg feature"
                fail=1
            fi
        done
        features=$(printf '%s\n' "$tree" | sed -n "s/^[^a-z]*$fenced_pkg feature \"\([a-z-]*\)\".*/\1/p" | sort -u | paste -sd, -)
        resolver="${resolver}workspace normal-edge $fenced_pkg features = {${features:-none}}; "
    done
    resolver="${resolver}infinityd, inf clean"
fi

scope="${#manifests[@]} manifests scanned, $forwarders forwarder feature(s) declared; resolver: $resolver"
if [ "$fail" -ne 0 ]; then
    echo "shipping-feature fence FAILED ($scope)"
    echo "Move the request to [dev-dependencies], or behind a non-default feature the DST recipes"
    echo "enable explicitly (inf-sim: -p inf-sim --features dst). Never on a normal edge (ADR-0107)."
    exit 1
fi
echo "shipping-feature fence OK ($scope)"
