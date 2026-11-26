$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'build-tools.ps1')

$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("versadown-build-tools-test-" + [guid]::NewGuid())
$originalPath = $env:Path
$originalUserProfile = $env:USERPROFILE
$originalVcvarsPath = $env:VCVARS_PATH
$originalRustupHome = $env:RUSTUP_HOME
try {
    $binDir = Join-Path $testRoot 'bin'
    New-Item -ItemType Directory -Force -Path $binDir | Out-Null
    $pnpmPath = Join-Path $binDir 'pnpm.cmd'
    Set-Content -LiteralPath $pnpmPath -Value "@echo off`r`necho 10.0.0`r`nexit /b 0" -NoNewline
    $fallbackPnpm = Join-Path $testRoot '.cache\codex-runtimes\codex-primary-runtime\dependencies\bin\fallback\pnpm.cmd'
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $fallbackPnpm) | Out-Null
    Set-Content -LiteralPath $fallbackPnpm -Value "@echo off`r`necho 11.0.0`r`nexit /b 0" -NoNewline
    $env:USERPROFILE = $testRoot
    $env:RUSTUP_HOME = Join-Path $originalUserProfile '.rustup'
    $cargoBinDir = Join-Path $originalUserProfile '.cargo\bin'
    $env:Path = "$binDir;$cargoBinDir;$originalPath"

    $tools = Get-VersaDownBuildTools -ProjectRoot $testRoot
    if ($tools.Pnpm -ne $pnpmPath) {
        throw "Expected pnpm from PATH, got $($tools.Pnpm)"
    }

    $vcvarsPath = Join-Path $testRoot 'vcvars64.bat'
    Set-Content -LiteralPath $vcvarsPath -Value '@echo off' -NoNewline
    $env:VCVARS_PATH = $vcvarsPath
    if ((Get-VersaDownMsvcEnvironment) -ne $vcvarsPath) {
        throw 'Expected the explicit MSVC environment path to be selected.'
    }

    Write-Host 'PASS: PATH pnpm takes priority over bundled fallback; explicit MSVC path is discovered.'
} finally {
    $env:Path = $originalPath
    $env:USERPROFILE = $originalUserProfile
    $env:VCVARS_PATH = $originalVcvarsPath
    $env:RUSTUP_HOME = $originalRustupHome
    if (Test-Path -LiteralPath $testRoot) {
        $resolvedTestRoot = [System.IO.Path]::GetFullPath($testRoot)
        $tempRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath()).TrimEnd([System.IO.Path]::DirectorySeparatorChar) + [System.IO.Path]::DirectorySeparatorChar
        if (-not $resolvedTestRoot.StartsWith($tempRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw 'Refusing to remove a test directory outside the temporary root.'
        }
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    }
}
