set shell := ["bash", "-eu", "-o", "pipefail", "-c"]
set windows-shell := ["powershell.exe", "-NoProfile", "-Command"]

mock-bin := justfile_directory() + "/target/mock-tools/bin"
windows-mock-bin := justfile_directory() + "/target/mock-tools"

mock-tools:
    ./scripts/install-mock-tools.sh

ci: mock-tools
    cargo fmt --all -- --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --locked
    # Real-tool tests launch genuine claude/codex processes (each tens of threads,
    # with two alive at once during the fork phase). On a dev box already running
    # agents this can brush the soft nproc limit and make the tool's own hook
    # `posix_spawn` fail with EAGAIN. Raise the soft limit to the hard ceiling for
    # these lines so the tests aren't flaky against a busy machine.
    ulimit -Su "$(ulimit -Hu)" && PATH="{{mock-bin}}:$PATH" cargo test --locked --test real_tool_codex -- --ignored --nocapture --test-threads=1
    ulimit -Su "$(ulimit -Hu)" && PATH="{{mock-bin}}:$PATH" cargo test --locked --test real_tool_claude -- --ignored --nocapture --test-threads=1
    ulimit -Su "$(ulimit -Hu)" && PATH="{{mock-bin}}:$PATH" cargo test --locked --test test_relay_roundtrip -- --ignored --nocapture --test-threads=1

# Match the native Windows CI gate. The package smoke copy is deliberately
# renamed instead of using a second Cargo target directory.
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
    just package-smoke-windows
    just real-tool-tests-windows

[windows]
package-smoke-windows:
    cargo build --release --locked
    New-Item -ItemType Directory -Force target/package-smoke | Out-Null
    # Move (not copy): real-tool-tests-windows runs next and every test-spawned
    # hcom process sets HCOM_DEV_ROOT, which makes dev_root_binary() pick
    # whichever of target/release or target/debug has the newer mtime. Leaving
    # a freshly-built target/release/hcom.exe behind would make it win over the
    # debug binary cargo test just built, so tests would silently re-exec into
    # this release build instead of exercising their own binary.
    Move-Item -Force target/release/hcom.exe target/package-smoke/hcom-windows-x86_64.exe
    $version = & target/package-smoke/hcom-windows-x86_64.exe --version; if ($LASTEXITCODE -ne 0 -or $version -notmatch '^hcom ') { throw "Packaged binary smoke test failed: $version" }; Write-Output $version
