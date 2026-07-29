set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
set windows-shell := ["powershell.exe", "-NoProfile", "-Command"]

windows-mock-bin := justfile_directory() + "/target/mock-tools"
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

mock-tools:
    HCOM_MOCK_TOOLS_PREFIX="{{mock-prefix}}" HCOM_MOCK_TOOLS_NPM_CACHE="{{mock-cache}}" bash ./scripts/install-mock-tools.sh

typecheck:
    bash ./scripts/typecheck.sh

dist-check:
    dist generate --check

# Each check's full output goes to a per-step log under {{ci-tmp}}/ci-logs/; stdout
# only shows one ok/FAILED line per step so a pass is scannable and a failure
# points straight at the relevant log instead of requiring re-runs or grepping.
# Optional args name the only steps to run, e.g. `just ci real_tool_claude`.
[unix]
ci *steps:
    #!/usr/bin/env bash
    set -uo pipefail
    only_steps="{{steps}}"
    mkdir -p "{{ci-tmp}}"
    log_dir="{{ci-tmp}}/ci-logs"
    mkdir -p "$log_dir"
    step() {
        local name="$1"; shift
        if [[ -n "$only_steps" && " $only_steps " != *" $name "* ]]; then
            return
        fi
        local log="$log_dir/$name.log"
        printf '[ci] %-20s ' "$name"
        if "$@" > "$log" 2>&1; then
            echo "ok"
        else
            local rc=$?
            echo "FAILED (exit $rc)"
            echo "----- last 40 lines: $log -----"
            tail -n 40 "$log"
            exit "$rc"
        fi
    }
    step dist-check  dist generate --check
    step typecheck   bash ./scripts/typecheck.sh
    step fmt         env TMPDIR="{{ci-tmp}}" cargo fmt --all -- --check
    step clippy      env TMPDIR="{{ci-tmp}}" cargo clippy --all-targets --locked -- -D warnings
    step test        env TMPDIR="{{ci-tmp}}" cargo test --locked
    # Real-tool tests launch genuine claude/codex processes (each tens of threads,
    # with two alive at once during the fork phase). On a dev box already running
    # agents this can brush the soft nproc limit and make the tool's own hook
    # `posix_spawn` fail with EAGAIN. Raise the soft limit to the hard ceiling for
    # these steps so the tests aren't flaky against a busy machine.
    ulimit -Su "$(ulimit -Hu)"
    step mock-tools             env HCOM_MOCK_TOOLS_PREFIX="{{mock-prefix}}" HCOM_MOCK_TOOLS_NPM_CACHE="{{mock-cache}}" bash ./scripts/install-mock-tools.sh
    step real_tool_codex        env TMPDIR="{{ci-tmp}}" PATH="{{mock-bin}}:$PATH" cargo test --locked --test real_tool_codex -- --ignored --nocapture --test-threads=1
    step real_tool_claude       env TMPDIR="{{ci-tmp}}" PATH="{{mock-bin}}:$PATH" cargo test --locked --test real_tool_claude -- --ignored --nocapture --test-threads=1
    step test_relay_roundtrip  env TMPDIR="{{ci-tmp}}" PATH="{{mock-bin}}:$PATH" cargo test --locked --test test_relay_roundtrip -- --ignored --nocapture --test-threads=1
    echo "[ci] all checks passed"

# Run every normal Windows CI check locally. The release-package smoke remains
# available separately for release validation.
[windows]
mock-tools-windows:
    & "{{justfile_directory()}}/scripts/install-mock-tools.ps1"

[windows]
real-tool-tests-windows: mock-tools-windows
    $env:PATH = "{{windows-mock-bin}};" + $env:PATH; cargo test --locked --test real_tool_codex -- --ignored --nocapture --test-threads=1
    $env:PATH = "{{windows-mock-bin}};" + $env:PATH; cargo test --locked --test real_tool_claude -- --ignored --nocapture --test-threads=1
    $env:PATH = "{{windows-mock-bin}};" + $env:PATH; cargo test --locked --test test_relay_roundtrip -- --ignored --nocapture --test-threads=1

# Mirrors `just ci`: each step's full output goes to a per-step log under
# target/ci-logs/, stdout shows one ok/FAILED line per step. Without this a
# failing pre-commit run buries the one relevant assertion under a full
# --nocapture real-tool transcript. Optional args name the only steps to run,
# e.g. `just ci real_tool_claude`.
[windows]
ci *steps:
    & "{{justfile_directory()}}/scripts/ci-windows.ps1" {{ if steps == "" { "" } else { "-Only " + steps } }}

[windows]
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
