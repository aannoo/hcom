set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

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
