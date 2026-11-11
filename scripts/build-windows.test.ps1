$ErrorActionPreference = 'Stop'

function Assert-True {
    param(
        [Parameter(Mandatory)]
        [bool]$Condition,

        [Parameter(Mandatory)]
        [string]$Message
    )

    if (-not $Condition) {
        throw $Message
    }
}

function Write-TestFile {
    param(
        [Parameter(Mandatory)]
        [string]$Path,

        [Parameter(Mandatory)]
        [string]$Content
    )

    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Path) | Out-Null
    Set-Content -LiteralPath $Path -Value $Content -NoNewline
}

$scriptSource = Get-Content (Join-Path $PSScriptRoot 'build-windows.ps1') -Raw
$functionsStart = $scriptSource.IndexOf('function Get-FileSha256')
$functionsEnd = $scriptSource.IndexOf('function Publish-BuildOutput')
if ($functionsStart -lt 0 -or $functionsEnd -lt 0) {
    throw 'Could not load the quick-build cache helpers.'
}
Invoke-Expression $scriptSource.Substring($functionsStart, $functionsEnd - $functionsStart)

$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("versadown-build-test-" + [guid]::NewGuid())
try {
    foreach ($relativePath in @(
        'index.html',
        'package.json',
        'pnpm-lock.yaml',
        'scripts/build-windows.ps1',
        'src/App.tsx',
        'src-tauri/Cargo.toml',
        'src-tauri/Cargo.lock',
        'src-tauri/build.rs',
        'src-tauri/tauri.conf.json',
        'src-tauri/tauri.windows.conf.json'
    )) {
        Write-TestFile -Path (Join-Path $testRoot $relativePath) -Content "before-$relativePath"
    }

    $targetRoot = Join-Path $testRoot 'target'
    $baselineFrontendSignature = Get-FrontendSignature -ProjectRoot $testRoot
    $baselineSignature = Get-QuickRustSignature -ProjectRoot $testRoot
    Save-QuickRustSignature -ProjectRoot $testRoot -Signature $baselineSignature

    Write-TestFile -Path (Join-Path $testRoot 'src/App.tsx') -Content 'after-frontend-change'
    $changedFrontendSignature = Get-FrontendSignature -ProjectRoot $testRoot
    $changedSignature = Get-QuickRustSignature -ProjectRoot $testRoot
    Assert-True -Condition ($baselineFrontendSignature -ne $changedFrontendSignature) -Message 'The frontend signature must change when frontend source changes.'
    Assert-True -Condition ($baselineSignature -ne $changedSignature) -Message 'The quick-build signature must change when frontend source changes.'
    Write-TestFile -Path (Join-Path $testRoot 'src-tauri/tauri.windows.conf.json') -Content 'after-windows-config-change'
    $changedWindowsConfigSignature = Get-QuickRustSignature -ProjectRoot $testRoot
    Assert-True -Condition ($changedSignature -ne $changedWindowsConfigSignature) -Message 'The quick-build signature must change when the Windows bundle configuration changes.'
    $fakeCargo = Join-Path $testRoot 'fake-cargo.cmd'
    $cleanMarker = Join-Path $testRoot 'cargo-clean-called.txt'
    Write-TestFile -Path $fakeCargo -Content "@echo off`r`necho %* > `"$cleanMarker`"`r`nexit /b 0"

    Reset-QuickRustBuildIfInputsChanged -CargoCommand $fakeCargo -ProjectRoot $testRoot -TargetRoot $targetRoot | Out-Null

    Assert-True -Condition (-not (Test-Path -LiteralPath $cleanMarker)) -Message 'A frontend-only change must not delete the incremental Rust target.'

    $blocked = $false
    try {
        Assert-PublishedBinaryAvailable -PublishedBinary (Join-Path $testRoot 'artifacts/quick/versadown.exe') -Processes @([pscustomobject]@{
            Id = 77
            Path = (Join-Path $testRoot 'artifacts/quick/versadown.exe')
        })
    } catch {
        $blocked = $_.Exception.Message -match 'PID 77'
    }
    Assert-True -Condition $blocked -Message 'The build must stop before compiling when its published executable is running.'

    $alternatePublish = & (Join-Path $PSScriptRoot 'build-windows.ps1') -Mode Quick -QuickPublishName quick-next -DryRun 6>&1
    Assert-True -Condition (($alternatePublish -join "`n") -match 'artifacts[\\/]quick-next[\\/]versadown\.exe') -Message 'A quick build must be able to publish beside a running executable.'

    $projectRoot = $testRoot
    $vcvars = Join-Path $testRoot 'vcvars64.bat'
    $compileMarker = Join-Path $testRoot 'frontend-signature.txt'
    $compileCargo = Join-Path $testRoot 'compile-cargo.cmd'
    Write-TestFile -Path $vcvars -Content '@echo off'
    Write-TestFile -Path $compileCargo -Content "@echo off`r`necho %VERSADOWN_FRONTEND_SIGNATURE% > `"$compileMarker`"`r`nexit /b 0"

    Invoke-MsvcBuild -VcvVars $vcvars -CargoCommand $compileCargo -PnpmCommand 'pnpm' -TargetRoot $targetRoot -Profile quick -FrontendSignature $changedFrontendSignature

    Assert-True -Condition ((Get-Content -LiteralPath $compileMarker -Raw).Trim() -eq $changedFrontendSignature) -Message 'The quick Cargo build must receive the current frontend signature.'

    $installerMarker = Join-Path $testRoot 'installer-args.txt'
    $fakePnpm = Join-Path $testRoot 'fake-pnpm.cmd'
    Write-TestFile -Path $fakePnpm -Content "@echo off`r`necho %* > `"$installerMarker`"`r`nexit /b 0"
    Invoke-MsvcBuild -VcvVars $vcvars -CargoCommand $compileCargo -PnpmCommand $fakePnpm -TargetRoot $targetRoot -Profile release -FrontendSignature $changedFrontendSignature -Installer
    Assert-True -Condition ((Get-Content -LiteralPath $installerMarker -Raw) -match 'exec tauri build --bundles nsis --config \.tools/video-bundle\.config\.json --runner') -Message 'The EXE installer build must use Windows-only resources and package NSIS without running the MSI bundler.'

    # Cargo/linker helpers may leave a descendant alive after the build command
    # itself exits. The build script must wait for that direct command, not the
    # entire descendant tree, or a completed build can hang before publishing.
    $backgroundCargo = Join-Path $testRoot 'background-cargo.cmd'
    Write-TestFile -Path $backgroundCargo -Content "@echo off`r`nstart `"`" /b powershell.exe -NoProfile -Command `"Start-Sleep -Seconds 4`"`r`nexit /b 0"
    $buildTimer = [System.Diagnostics.Stopwatch]::StartNew()
    Invoke-MsvcBuild -VcvVars $vcvars -CargoCommand $backgroundCargo -PnpmCommand 'pnpm' -TargetRoot $targetRoot -Profile quick -FrontendSignature $changedFrontendSignature
    $buildTimer.Stop()
    Assert-True -Condition ($buildTimer.Elapsed.TotalSeconds -lt 2) -Message 'The quick build must not wait for detached compiler descendants after the build command exits.'

    Write-Host 'PASS: frontend-only changes preserve the incremental Rust target.'
} finally {
    if (Test-Path -LiteralPath $testRoot) {
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    }
}
