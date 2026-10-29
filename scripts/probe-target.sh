# Sourced by check-clock-ban.sh and check-lint-scopes.sh (ADR-0106 D18): ONE
# answer to "which target does a planted-bypass probe compile for, and which
# of the config entries Clippy could not resolve there are deferred to
# another leg", so the architecture a gate discloses is the one Clippy
# resolved against. check-waker-atomics.sh (ADR-0106 D8) sources it for the
# one target its tree asm and its probe compile for. Not executable on its
# own. Portable bash 3.2.

# The architectures a build-test leg of .github/workflows/infinity-ci.yml
# compiles for; the scripts self-test holds the workflow to this list.
PROBE_LEG_ARCHES="x86_64 aarch64"
# The targets the waker gate is enforced on: it runs on the Linux legs only
# (ADR-0106 D8), and an atomic is spelled per target — aarch64 Linux calls
# the outline-atomics helpers where Apple inlines LSE — so each needs its
# own Linux leg; a leg sharing only the architecture does not stand in. The
# scripts self-test holds the workflow to this list.
WAKER_LEG_TARGETS="x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu"

# probe_target [<toolchain>]: sets PROBE_TARGET (CARGO_BUILD_TARGET, else the
# toolchain's host) and PROBE_ARCH (rustc's `target_arch` for it — not the
# triple's prefix: arm64e-apple-darwin is aarch64). Returns 1 after an
# explanation on stderr when rustc names neither, or when the host's
# architecture disagrees with `uname -m` (a wrong arch would pass a misspelled
# entry of this leg's own module as foreign).
probe_target() {
    local tc=${1:+"+$1"} machine
    PROBE_TARGET=${CARGO_BUILD_TARGET:-}
    if [ -z "$PROBE_TARGET" ]; then
        PROBE_TARGET=$(rustc $tc -vV | sed -n 's/^host: //p') || PROBE_TARGET=
    fi
    PROBE_ARCH=
    if [ -n "$PROBE_TARGET" ]; then
        PROBE_ARCH=$(rustc $tc --print cfg --target "$PROBE_TARGET" |
            sed -n 's/^target_arch="\(.*\)"$/\1/p') || PROBE_ARCH=
    fi
    if [ -z "$PROBE_ARCH" ]; then
        echo "probe-target: rustc${tc:+ $tc} names no target_arch for target '$PROBE_TARGET'" >&2
        return 1
    fi
    [ -z "${CARGO_BUILD_TARGET:-}" ] || return 0
    machine=$(uname -m) || machine=
    case $machine in arm64) machine=aarch64 ;; amd64) machine=x86_64 ;; esac
    if [ "$machine" != "$PROBE_ARCH" ]; then
        echo "probe-target: rustc's host $PROBE_TARGET is $PROBE_ARCH, which disagrees with uname -m ($machine)" >&2
        return 1
    fi
}

# probe_foreign <config path>: true when the path is under another leg's
# `core::arch` module, which cannot resolve for PROBE_ARCH and is enforced on
# that leg. Any other path that resolves to nothing is red on every leg —
# PROBE_ARCH's own module and a misspelled one (`x86_46`) alike.
probe_foreign() {
    local arch
    case $1 in core::arch::*::* | std::arch::*::*) ;; *) return 1 ;; esac
    arch=${1#*::arch::}
    arch=${arch%%::*}
    [ "$arch" != "$PROBE_ARCH" ] || return 1
    case " $PROBE_LEG_ARCHES " in *" $arch "*) return 0 ;; esac
    return 1
}
