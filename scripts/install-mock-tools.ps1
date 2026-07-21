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
        "@openai/codex@0.141.0",
        "@anthropic-ai/claude-code@2.1.216"
    )
}

New-Item -ItemType Directory -Force $prefix, $cache | Out-Null

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

# npm's global executable directory is <prefix> on Windows and <prefix>/bin
# on Unix. Print it so callers can add the exact directory to PATH.
Write-Output $prefix
