set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
set windows-shell := ["powershell.exe", "-NoProfile", "-Command"]

# Where the pinned real CLIs (claude/codex/...) that the real-tool tests drive
# get installed, plus the npm cache they are installed through. On Android both
# move under $HOME: npm links its shims with symlinks, and the checkout's FUSE
# mount rejects those (`ln -s` under the repo fails with EPERM).
default-mock-prefix := if os() == "android" {
    home_directory() + "/.cache/hcom-mock-tools"
} else {
    justfile_directory() + "/target/mock-tools"
}

default-mock-cache := if os() == "android" {
    home_directory() + "/.cache/hcom-mock-tools-npm"
} else {
    justfile_directory() + "/target/npm-cache"
}

# Holds `just ci`'s per-step logs, and is handed to every cargo step as TMPDIR
# so test temp dirs land in one predictable place. Off Android this resolves to
# the ambient TMPDIR and so changes nothing; on Android it is pinned under
# $HOME/.cache rather than Termux's $PREFIX/tmp.
ci-tmp := if os() == "android" {
    home_directory() + "/.cache/hcom-test-tmp"
} else {
    env_var_or_default("TMPDIR", "/tmp")
}

mock-prefix := env_var_or_default(
    "HCOM_MOCK_TOOLS_PREFIX",
    default-mock-prefix,
)
mock-cache := env_var_or_default("HCOM_MOCK_TOOLS_NPM_CACHE", default-mock-cache)
mock-bin := mock-prefix + "/bin"

# The Windows recipes and scripts/ci-windows.ps1 keep their own copies of these
# paths: the pinned tools install to the npm prefix root there, not prefix/bin.
windows-mock-bin := justfile_directory() + "/target/mock-tools"
windows-log-dir := justfile_directory() + "/target/ci-logs"

# Bare `just` lists recipes instead of running whichever one is declared first.
[private]
default:
    @just --list --unsorted

[group("setup")]
[doc("Install the pinned real CLIs that the real-tool tests run against")]
mock-tools:
    HCOM_MOCK_TOOLS_PREFIX="{{mock-prefix}}" HCOM_MOCK_TOOLS_NPM_CACHE="{{mock-cache}}" bash ./scripts/install-mock-tools.sh

[group("checks")]
[doc("Typecheck the omp/opencode/pi plugin sources (skips loudly on Android)")]
typecheck:
    bash ./scripts/typecheck.sh

[group("checks")]
[doc("Check the committed release workflow still matches dist-workspace.toml")]
dist-check:
    dist generate --check

[group("checks")]
[doc("cargo check on the MSRV from Cargo.toml (skips loudly if not installed)")]
msrv:
    bash ./scripts/msrv.sh

[group("checks")]
[doc("Apply rustfmt (the ci fmt step only checks)")]
fmt:
    cargo fmt --all

[unix]
[group("checks")]
[doc("cargo test under the ci TMPDIR; extra args are forwarded to cargo")]
test *args:
    env TMPDIR="{{ci-tmp}}" cargo test --locked {{ args }}

[unix]
[group("checks")]
[doc("Run every local check; args limit the run to the named steps")]
ci *steps:
    #!/usr/bin/env bash
    # Each step's full output goes to a per-step log; stdout gets one ok/FAILED
    # line per step, so a pass is scannable and a failure points straight at the
    # relevant log instead of requiring a re-run or a grep. Without this a
    # failing pre-commit run buries the one relevant assertion under a full
    # --nocapture real-tool transcript.
    set -uo pipefail
    only_steps="{{ steps }}"

    export TMPDIR="{{ ci-tmp }}"
    export HCOM_MOCK_TOOLS_PREFIX="{{ mock-prefix }}"
    export HCOM_MOCK_TOOLS_NPM_CACHE="{{ mock-cache }}"

    log_dir="$TMPDIR/ci-logs"
    mkdir -p "$log_dir"
    echo "[ci] logs: $log_dir/<step>.log"

    declare -a known=()
    ran=0
    skipped=0
    step() {
        local name="$1"; shift
        # Recorded before the filter, so the unknown-step check at the end sees
        # every step name whether or not this run executed it.
        known+=("$name")
        if [[ -n "$only_steps" && " $only_steps " != *" $name "* ]]; then
            return
        fi
        local log="$log_dir/$name.log"
        local start=$SECONDS
        printf '[ci] %-20s ' "$name"
        if "$@" > "$log" 2>&1; then
            # A skipped check must not report as a pass: typecheck opts out on
            # Android, msrv without the toolchain, and both say so on stdout.
            if grep -qE '(^|: )SKIPPED\b' "$log"; then
                printf 'skipped (%ds)\n' "$((SECONDS - start))"
                skipped=$((skipped + 1))
            else
                printf 'ok (%ds)\n' "$((SECONDS - start))"
                ran=$((ran + 1))
            fi
        else
            local rc=$?
            printf 'FAILED (exit %d, %ds)\n' "$rc" "$((SECONDS - start))"
            echo "----- last 40 lines: $log -----"
            tail -n 40 "$log"
            exit "$rc"
        fi
    }

    step dist-check dist generate --check
    step typecheck  bash ./scripts/typecheck.sh
    step fmt        cargo fmt --all -- --check
    step clippy     cargo clippy --all-targets --locked -- -D warnings
    step test       cargo test --locked
    step msrv       bash ./scripts/msrv.sh

    # Real-tool tests launch genuine claude/codex processes (each tens of
    # threads, with two alive at once during the fork phase). On a dev box
    # already running agents this can brush the soft nproc limit and make the
    # tool's own hook `posix_spawn` fail with EAGAIN. Raise the soft limit to the
    # hard ceiling for these steps so the tests aren't flaky against a busy
    # machine.
    ulimit -Su "$(ulimit -Hu)" 2>/dev/null || true
    step mock-tools bash ./scripts/install-mock-tools.sh
    # Only after mock-tools has populated it: the pinned CLIs must outrank any
    # ambient install of the same tool for the steps below.
    export PATH="{{ mock-bin }}:$PATH"
    step real_tool_codex      cargo test --locked --test real_tool_codex -- --ignored --nocapture --test-threads=1
    step real_tool_claude     cargo test --locked --test real_tool_claude -- --ignored --nocapture --test-threads=1
    step test_relay_roundtrip cargo test --locked --test test_relay_roundtrip -- --ignored --nocapture --test-threads=1

    for want in $only_steps; do
        if [[ " ${known[*]} " != *" $want "* ]]; then
            echo "[ci] unknown step: $want" >&2
            echo "[ci] steps: ${known[*]}" >&2
            exit 2
        fi
    done
    if (( skipped > 0 )); then
        printf '[ci] %d step(s) passed, %d skipped, in %ds\n' "$ran" "$skipped" "$SECONDS"
    else
        printf '[ci] %d step(s) passed in %ds\n' "$ran" "$SECONDS"
    fi

[unix]
[group("checks")]
[doc("Print where ci logs live, or the log of one named step")]
ci-logs step="":
    #!/usr/bin/env bash
    set -uo pipefail
    log_dir="{{ ci-tmp }}/ci-logs"
    if [[ -n "{{ step }}" ]]; then
        exec cat "$log_dir/{{ step }}.log"
    fi
    echo "[ci] logs: $log_dir"
    ls -1 "$log_dir" 2>/dev/null | sed 's/\.log$//' || echo "[ci] (none yet - run just ci)"

[windows]
[group("setup")]
[doc("Install the pinned real CLIs that the real-tool tests run against")]
mock-tools-windows:
    & "{{ justfile_directory() }}/scripts/install-mock-tools.ps1"

[windows]
[group("checks")]
[doc("Run just the real-tool tests against the pinned CLIs")]
real-tool-tests-windows: mock-tools-windows
    $env:PATH = "{{ windows-mock-bin }};" + $env:PATH; cargo test --locked --test real_tool_codex -- --ignored --nocapture --test-threads=1
    $env:PATH = "{{ windows-mock-bin }};" + $env:PATH; cargo test --locked --test real_tool_claude -- --ignored --nocapture --test-threads=1
    $env:PATH = "{{ windows-mock-bin }};" + $env:PATH; cargo test --locked --test test_relay_roundtrip -- --ignored --nocapture --test-threads=1

# Mirrors the unix `ci` recipe's reporting. The steps live in a script rather
# than inline because just writes shebang recipe bodies to an extension-less
# temp file, which `powershell -File` refuses. Names are comma-joined because
# the script takes them as a single [string[]] parameter.
[windows]
[group("checks")]
[doc("Run every local check; args limit the run to the named steps")]
ci *steps:
    & "{{ justfile_directory() }}/scripts/ci-windows.ps1" {{ if steps == "" { "" } else { "-Only " + replace(steps, " ", ",") } }}

[windows]
[group("checks")]
[doc("Print where ci logs live, or the log of one named step")]
ci-logs step="":
    if ("{{ step }}") { Get-Content "{{ windows-log-dir }}/{{ step }}.log" } else { Write-Output "[ci] logs: {{ windows-log-dir }}"; Get-ChildItem "{{ windows-log-dir }}" -ErrorAction SilentlyContinue | ForEach-Object { $_.BaseName } }

# Kept out of `ci`: this is release validation, not a pre-commit check.
[windows]
[group("release")]
[doc("Build the release binary, package it, and smoke-test --version")]
package-smoke-windows:
    cargo build --release --locked
    New-Item -ItemType Directory -Force target/package-smoke | Out-Null
    # Move (not copy): if real-tool-tests-windows runs after this, every
    # test-spawned hcom process sets HCOM_DEV_ROOT, which makes dev_root_binary() pick
    # whichever of target/release or target/debug has the newer mtime. Leaving
    # a freshly-built target/release/hcom.exe behind would make it win over the
    # debug binary cargo test just built, so tests would silently re-exec into
    # this release build instead of exercising their own binary.
    Move-Item -Force target/release/hcom.exe target/package-smoke/hcom-windows-x86_64.exe
    $version = & target/package-smoke/hcom-windows-x86_64.exe --version; if ($LASTEXITCODE -ne 0 -or $version -notmatch '^hcom ') { throw "Packaged binary smoke test failed: $version" }; Write-Output $version
