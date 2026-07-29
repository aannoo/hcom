# Every normal Windows CI check, run locally as the pre-commit gate.
#
# Mirrors the `ci` recipe's reporting: each step's full output goes to a
# per-step log under target/ci-logs/, stdout shows one ok/FAILED line per step.
# Without that, a failing pre-commit run buries the one relevant assertion under
# a full --nocapture real-tool transcript.
#
# Lives here rather than inline in the Justfile because just writes shebang
# recipe bodies to an extension-less temp file, which powershell -File refuses.
param(
    [string[]] $Only
)

$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot
$logDir = Join-Path $root "target/ci-logs"
New-Item -ItemType Directory -Force $logDir | Out-Null

# The pinned real tools install to the npm prefix root on Windows (not
# prefix/bin as on Unix), and must outrank any ambient install of the same tool.
$env:PATH = (Join-Path $root "target/mock-tools") + ";" + $env:PATH

# Agents launched from this checkout keep target/debug/hcom.exe mapped, and
# Windows refuses to delete a mapped image — so cargo fails with "Access is
# denied" before it compiles anything. Renaming a running executable IS allowed
# (the mapping follows the inode), which frees the name for the new build while
# live agents keep running off the renamed file. Preferred over killing the
# agents or building into a temp target dir: neither leaves the dev_root binary
# that `hcom` resolves to pointing at what was just built.
function Unlock-DevBinary {
    $binary = Join-Path $root "target/debug/hcom.exe"
    if (-not (Test-Path -PathType Leaf $binary)) { return }
    # Sweep previous parks first; they are only removable once the agents that
    # were holding them have exited, so failures here are expected and ignored.
    Get-ChildItem (Join-Path $root "target/debug") -Filter "hcom.exe.locked-*" -ErrorAction SilentlyContinue |
        ForEach-Object { try { Remove-Item -Force $_.FullName -ErrorAction Stop } catch {} }
    # Park unconditionally rather than probing first: hooks of live agents
    # execute hcom constantly, so "nothing holds it right now" says nothing
    # about the moment cargo tries to unlink it a few seconds later.
    $parked = "hcom.exe.locked-{0}" -f (Get-Date -Format "yyyyMMddHHmmss")
    try {
        Rename-Item $binary $parked -ErrorAction Stop
        Write-Host "[ci] parked locked dev binary as $parked"
    } catch {
        Write-Host "[ci] warning: target/debug/hcom.exe is locked and could not be renamed"
    }
}

function Step([string] $name, [scriptblock] $body) {
    if ($Only -and $Only -notcontains $name) { return }
    $log = Join-Path $logDir "$name.log"
    Write-Host ("[ci] {0,-20} " -f $name) -NoNewline
    # Reset first: a step whose body runs no native command would otherwise be
    # judged by whatever the previous step's last process returned.
    $global:LASTEXITCODE = 0
    $code = 0
    try {
        & $body *> $log
        $code = $LASTEXITCODE
    } catch {
        # install-mock-tools.ps1 reports a failed pin by throwing, which sets no
        # exit code. Without this, the whole run would unwind with the message
        # buried in $log and no FAILED line naming the step.
        $_ | Out-String | Add-Content $log
        $code = 1
    }
    if ($code -ne 0) {
        Write-Host "FAILED (exit $code)"
        Write-Host "----- last 40 lines: $log -----"
        Get-Content $log -Tail 40
        exit $code
    }
    Write-Host "ok"
}

Unlock-DevBinary

Step fmt        { cargo fmt --all -- --check }
Step clippy     { cargo clippy --all-targets --locked -- -D warnings }
Step test       { cargo test --all-targets --locked }
Step mock-tools { & (Join-Path $PSScriptRoot "install-mock-tools.ps1") }
Step real_tool_codex      { cargo test --locked --test real_tool_codex -- --ignored --nocapture --test-threads=1 }
Step real_tool_claude     { cargo test --locked --test real_tool_claude -- --ignored --nocapture --test-threads=1 }
Step test_relay_roundtrip { cargo test --locked --test test_relay_roundtrip -- --ignored --nocapture --test-threads=1 }

Write-Host "[ci] all checks passed"
