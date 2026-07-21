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

ci: mock-tools typecheck
    mkdir -p "{{ci-tmp}}"
    TMPDIR="{{ci-tmp}}" cargo fmt --all -- --check
    TMPDIR="{{ci-tmp}}" cargo clippy --all-targets --locked -- -D warnings
    TMPDIR="{{ci-tmp}}" cargo test --locked
    # Real-tool tests launch genuine claude/codex processes (each tens of threads,
    # with two alive at once during the fork phase). On a dev box already running
    # agents this can brush the soft nproc limit and make the tool's own hook
    # `posix_spawn` fail with EAGAIN. Raise the soft limit to the hard ceiling for
    # these lines so the tests aren't flaky against a busy machine.
    ulimit -Su "$(ulimit -Hu)" && TMPDIR="{{ci-tmp}}" PATH="{{mock-bin}}:$PATH" cargo test --locked --test real_tool_codex -- --ignored --nocapture --test-threads=1
    ulimit -Su "$(ulimit -Hu)" && TMPDIR="{{ci-tmp}}" PATH="{{mock-bin}}:$PATH" cargo test --locked --test real_tool_claude -- --ignored --nocapture --test-threads=1
    ulimit -Su "$(ulimit -Hu)" && TMPDIR="{{ci-tmp}}" PATH="{{mock-bin}}:$PATH" cargo test --locked --test test_relay_roundtrip -- --ignored --nocapture --test-threads=1

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

[windows]
ci-windows:
    cargo fmt --all -- --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --all-targets --locked
    just real-tool-tests-windows

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
