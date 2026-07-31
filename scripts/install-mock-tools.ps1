param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]] $Packages
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$prefix = if ($env:HCOM_MOCK_TOOLS_PREFIX) {
    $env:HCOM_MOCK_TOOLS_PREFIX
} else {
    Join-Path $root "target/mock-tools"
}
$cache = if ($env:HCOM_MOCK_TOOLS_NPM_CACHE) {
    $env:HCOM_MOCK_TOOLS_NPM_CACHE
} else {
    Join-Path $root "target/npm-cache"
}

if (-not $Packages -or $Packages.Count -eq 0) {
    $Packages = @(
        "@openai/codex@0.145.0",
        "@anthropic-ai/claude-code@2.1.216"
    )
}

New-Item -ItemType Directory -Force $prefix, $cache | Out-Null

# Resolve a tool launcher exactly as Windows does — extension-major within the
# directory, `.EXE` before `.CMD` — so this script, hcom's `which_bin`, and the
# tests' pin check can never disagree about which file they mean.
function Resolve-Launcher([string] $tool) {
    @(".com", ".exe", ".bat", ".cmd", "") |
        ForEach-Object { Join-Path $prefix "$tool$_" } |
        Where-Object { Test-Path -PathType Leaf $_ } |
        Select-Object -First 1
}

function Get-PinnedTools([string[]] $packages) {
    foreach ($package in $packages) {
        if ($package -notmatch '^(@[^/]+/)?([^@]+)@(.+)$') { continue }
        $name = $Matches[2]
        $tool = switch ($name) {
            "claude-code" { "claude" }
            default { $name }
        }
        [pscustomobject]@{ Package = $package; Tool = $tool; Version = $Matches[3] }
    }
}

# What each launcher currently reports, or $null if it is missing or unrunnable.
function Get-ReportedVersion([string] $tool) {
    $launcher = Resolve-Launcher $tool
    if (-not $launcher) { return $null }
    try {
        $reported = (& $launcher --version 2>&1 | Out-String).Trim()
    } catch {
        return $null
    }
    if ($LASTEXITCODE -ne 0 -or -not $reported) { return $null }
    [pscustomobject]@{ Launcher = $launcher; Reported = $reported }
}

$pinned = @(Get-PinnedTools $Packages)

# Skip the install when every pin is already satisfied. npm rewrites the whole
# package tree, which fails with EBUSY/EPERM if any agent still has the native
# binary mapped — and on a dev box `just windows-ci` is normally run with agents
# alive. A no-op install must not be the reason the gate cannot run. CI restores
# this prefix from a version-keyed cache, so it takes the same fast path.
$needsInstall = $false
foreach ($entry in $pinned) {
    $current = Get-ReportedVersion $entry.Tool
    if (-not $current -or $current.Reported -notmatch [regex]::Escape($entry.Version)) {
        $needsInstall = $true
    }
}

if ($needsInstall) {
    # npm rewrites only the launchers it owns (`claude`, `claude.cmd`,
    # `claude.ps1`) and leaves anything else in the prefix alone. Claude Code's
    # own installer has historically dropped a native `<tool>.exe` here, and it
    # outranks the shim npm is about to write in PATHEXT order — so one leftover
    # from an earlier pin silently takes over, and the resulting failure is a
    # version mismatch with nothing pointing at the stale file. Clear them first.
    foreach ($entry in $pinned) {
        foreach ($ext in @(".exe", ".com", ".bat")) {
            $stale = Join-Path $prefix "$($entry.Tool)$ext"
            if (Test-Path -PathType Leaf $stale) {
                Write-Output "removing stale launcher: $stale"
                Remove-Item -Force $stale
            }
        }
    }

    $npm = (Get-Command npm.cmd -ErrorAction Stop).Source
    & $npm install `
        --global `
        --prefix $prefix `
        --cache $cache `
        --no-audit `
        --no-fund `
        --fetch-retries 5 `
        --fetch-retry-mintimeout 20000 `
        --fetch-retry-maxtimeout 120000 `
        --fetch-timeout 600000 `
        @Packages
    if ($LASTEXITCODE -ne 0) {
        throw "npm install failed with exit code $LASTEXITCODE"
    }
}

# Verify the pin here rather than letting a real-tool test discover it: this
# script knows which versions it asked for and can name the file that answered,
# which a `found 2.1.185` panic 200 lines into a test cannot.
foreach ($entry in $pinned) {
    $current = Get-ReportedVersion $entry.Tool
    if (-not $current) {
        throw "installed $($entry.Package) but no runnable '$($entry.Tool)' launcher in $prefix"
    }
    if ($current.Reported -notmatch [regex]::Escape($entry.Version)) {
        throw "pinned $($entry.Package), but '$($current.Launcher)' reports '$($current.Reported)'"
    }
    Write-Output "$($entry.Tool) $($entry.Version) verified at $($current.Launcher)"
}

# npm's global executable directory is <prefix> on Windows and <prefix>/bin
# on Unix. Print it so callers can add the exact directory to PATH.
Write-Output $prefix
