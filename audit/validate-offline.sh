#!/usr/bin/env bash
# Requires an already-installed Rust 1.98.1 in an isolated prefix.
# Never installs a compiler, packages or plugins; never enables Cargo networking.
# Run ../setup.sh and source ../build-env.sh beforehand when using the buildenv.
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo"
free -h; df -h "$repo"; nproc
memory_status() {
    local f
    for f in memory.max memory.current memory.events; do
        printf '\n%s\n' "$f"; cat "/sys/fs/cgroup/$f" 2>/dev/null || true
    done
}
memory_status
for tool in python3 timeout; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        printf 'ENV: missing %s; no Rust build or test was run.\n' "$tool" >&2
        exit 78
    fi
done
rust_prefix=${BIGNETSCREEN_RUST_PREFIX:-/mnt/data/toolchains/rust-1.98.1}
rust_prefix=$(python3 -c 'import os,sys; print(os.path.abspath(sys.argv[1]))' "$rust_prefix")
run_dir=$(mktemp -d "$repo/../review-run.XXXXXXXX")
printf '\nIsolated test data and logs: %s\n' "$run_dir"
printf 'gate\texit_code\n' > "$run_dir/status.tsv"
run() {
    local name=$1; shift
    local result=0
    printf '\n== %s ==\n' "$name"
    timeout --signal=TERM --kill-after=30s 15m "$@" 2>&1 | tee "$run_dir/$name.log" || result=$?
    printf '%s\t%s\n' "$name" "$result" >> "$run_dir/status.tsv"
    # Always record cgroup evidence, including on a failing command/timeout.
    memory_status | tee "$run_dir/$name.memory.log"
    if (( result != 0 )); then
        printf 'Gate %s failed (exit %s); classify from its log before changing project code.\n' "$name" "$result" >&2
        return "$result"
    fi
}
run toolchain python3 audit/tools/check-rust-toolchain.py --prefix "$rust_prefix"
export PATH="$rust_prefix/bin:$PATH"
export CARGO="$rust_prefix/bin/cargo" RUSTC="$rust_prefix/bin/rustc"
export RUSTDOC="$rust_prefix/bin/rustdoc" RUSTFMT="$rust_prefix/bin/rustfmt"
# Prevent a caller's Cargo configuration from silently substituting a compiler.
export RUSTC_WRAPPER='' RUSTC_WORKSPACE_WRAPPER=''
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
case "$CARGO_BUILD_JOBS" in
    1|2) ;;
    *) printf 'ENV: this audit allows CARGO_BUILD_JOBS=1 or 2 only.\n' >&2; exit 78 ;;
esac
export RUST_TEST_THREADS=1 CARGO_NET_OFFLINE=true
# Preserve existing cache paths and build profiles; never run cargo clean.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo/target}"
export XDG_CONFIG_HOME="$run_dir/config" XDG_STATE_HOME="$run_dir/state"
export XDG_DATA_HOME="$run_dir/data" XDG_CACHE_HOME="$run_dir/cache" TMPDIR="$run_dir/tmp"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_STATE_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$TMPDIR"
# Reset all program overrides, including GOP/sync/drain values previously missed.
# Save the prefix before this loop; it is an audit setting, not application state.
for name in "${!BIGNETSCREEN_@}"; do unset "$name"; done
unset NETWORK_DISPLAYS_DUMMY
# Cheapest gates first. These execute actual Rust only after the exact-version check.
run formatting cargo fmt --all -- --check
run flow-compile rustc --edition=2021 --test crates/nd-chromecast/src/flow.rs -o "$run_dir/flow-tests"
run flow-tests "$run_dir/flow-tests" --test-threads=1
run core-native python3 audit/tools/probe-buildenv.py --scope core --work-dir "$TMPDIR"
run focused cargo test --offline --locked -p nd-chromecast -p nd-core -p nd-webrtc -p nd-net
run workspace-check cargo check --offline --locked --workspace --all-targets
# pkg-config metadata alone does not prove the GUI's actual libraries exist.
run workspace-native python3 audit/tools/probe-buildenv.py --scope workspace --work-dir "$TMPDIR"
run workspace-tests cargo test --offline --locked --workspace
run clippy cargo clippy --offline --locked --workspace --all-targets -- -D warnings
printf '\nAll invoked Rust gates passed. Receiver, GUI and hardware tests remain separate.\n'
