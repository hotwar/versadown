[CmdletBinding()]
param(
    [ValidateSet('Quick', 'Release')]
    [string]$Mode = 'Release',

    # Packaging is intentionally a release-only operation. Quick builds are
    # runnable local candidates, never installer artifacts.
    [switch]$Installer,

    # Publish a quick build beside an executable that is currently running.
    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9._-]*$')]
    [string]$QuickPublishName,

    # Lets CI and contributors verify the selected layout without invoking a
    # frontend, Rust, or WiX build.
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'

if ($Installer -and $Mode -ne 'Release') {
    throw 'Installer packaging is only available for the Release build mode.'
}
if ($QuickPublishName -and $Mode -ne 'Quick') {
    throw 'An alternate publish name is only available for the Quick build mode.'
}

$projectRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'build-tools.ps1')

$profile = if ($Mode -eq 'Quick') { 'quick' } else { 'release' }
$targetRoot = if ($Mode -eq 'Quick') {
    Join-Path $projectRoot '.tools\quick-target'
} else {
    Join-Path $projectRoot '.tools\release-target'
}
$publishedDir = if ($Mode -eq 'Quick') {
    Join-Path $projectRoot ("artifacts\{0}" -f $(if ($QuickPublishName) { $QuickPublishName } else { 'quick' }))
} else {
    Join-Path $projectRoot 'artifacts'
}
$publishedBinary = Join-Path $publishedDir 'versadown.exe'
$publishedSidecarDir = Join-Path $publishedDir 'binaries'
$sidecarName = 'aria2-next-x86_64-pc-windows-msvc.exe'
$sidecarSource = Join-Path $projectRoot "src-tauri\binaries\$sidecarName"
$publishedSidecar = Join-Path $publishedSidecarDir $sidecarName
$retiredXunleiHelper = Join-Path $publishedSidecarDir 'xunlei-native-helper.cjs'
$legacyPublishedSidecar = Join-Path $publishedSidecarDir 'aria2c-x86_64-pc-windows-msvc.exe'

function Get-FileSha256 {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$Path
    )

    $stream = [System.IO.File]::OpenRead($Path)
    $hasher = [System.Security.Cryptography.SHA256]::Create()
    try {
        return -join ($hasher.ComputeHash($stream) | ForEach-Object { $_.ToString('x2') })
    } finally {
        $hasher.Dispose()
        $stream.Dispose()
    }
}

function Get-FrontendSignature {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$ProjectRoot
    )

    $frontendInputs = @(
        (Join-Path $ProjectRoot 'index.html'),
        (Join-Path $ProjectRoot 'package.json'),
        (Join-Path $ProjectRoot 'pnpm-lock.yaml')
    )
    $frontendInputs += Get-ChildItem -LiteralPath (Join-Path $ProjectRoot 'src') -File -Recurse
    $frontendInputs += Get-ChildItem -LiteralPath $ProjectRoot -File -Filter 'tsconfig*.json'
    $frontendInputs += Get-ChildItem -LiteralPath $ProjectRoot -File -Filter 'vite.config.*'
    $publicDir = Join-Path $ProjectRoot 'public'
    if (Test-Path -LiteralPath $publicDir) {
        $frontendInputs += Get-ChildItem -LiteralPath $publicDir -File -Recurse
    }

    $signatureLines = foreach ($file in $frontendInputs | Sort-Object { $_.FullName }) {
        $path = if ($file -is [System.IO.FileInfo]) { $file.FullName } else { [string]$file }
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            continue
        }
        $relativePath = $path.Substring($ProjectRoot.Length).TrimStart([char[]]@([System.IO.Path]::DirectorySeparatorChar, [System.IO.Path]::AltDirectorySeparatorChar))
        '{0}:{1}' -f $relativePath, (Get-FileSha256 -Path $path)
    }
    $signatureBytes = [System.Text.Encoding]::UTF8.GetBytes(($signatureLines -join "`n"))
    $signatureHasher = [System.Security.Cryptography.SHA256]::Create()
    try {
        return -join ($signatureHasher.ComputeHash($signatureBytes) | ForEach-Object { $_.ToString('x2') })
    } finally {
        $signatureHasher.Dispose()
    }
}

function Save-FrontendSignature {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$ProjectRoot,

        [Parameter(Mandatory)]
        [string]$Signature
    )

    $cacheDir = Join-Path $ProjectRoot '.tools\build-cache'
    New-Item -ItemType Directory -Force -Path $cacheDir | Out-Null
    Set-Content -LiteralPath (Join-Path $cacheDir 'frontend.sha256') -Value $Signature -NoNewline
}

function Get-QuickRustSignature {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$ProjectRoot
    )

    $tauriRoot = Join-Path $ProjectRoot 'src-tauri'
    $rustInputs = @(
        (Join-Path $ProjectRoot 'scripts\build-windows.ps1'),
        (Join-Path $tauriRoot 'Cargo.toml'),
        (Join-Path $tauriRoot 'Cargo.lock'),
        (Join-Path $tauriRoot 'build.rs'),
        (Join-Path $tauriRoot 'tauri.conf.json'),
        (Join-Path $tauriRoot 'tauri.windows.conf.json'),
        (Join-Path $tauriRoot 'vendor\tao\Cargo.toml')
    )
    foreach ($sourceDirectory in @(
        (Join-Path $tauriRoot 'src'),
        (Join-Path $tauriRoot 'vendor\tao\src')
    )) {
        if (Test-Path -LiteralPath $sourceDirectory) {
            $rustInputs += Get-ChildItem -LiteralPath $sourceDirectory -File -Recurse
        }
    }

    $signatureLines = foreach ($file in $rustInputs | Sort-Object { $_.FullName }) {
        $path = if ($file -is [System.IO.FileInfo]) { $file.FullName } else { [string]$file }
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            continue
        }
        $relativePath = $path.Substring($ProjectRoot.Length).TrimStart([char[]]@([System.IO.Path]::DirectorySeparatorChar, [System.IO.Path]::AltDirectorySeparatorChar))
        '{0}:{1}' -f $relativePath, (Get-FileSha256 -Path $path)
    }
    # The GUI executable embeds the Vite output through Tauri's custom
    # protocol. Reusing a Rust target after only frontend files changed would
    # publish an executable with stale UI assets, so the quick target must
    # depend on the same source signature as the frontend cache.
    $signatureLines += 'frontend:{0}' -f (Get-FrontendSignature -ProjectRoot $ProjectRoot)
    $signatureBytes = [System.Text.Encoding]::UTF8.GetBytes(($signatureLines -join "`n"))
    $signatureHasher = [System.Security.Cryptography.SHA256]::Create()
    try {
        return -join ($signatureHasher.ComputeHash($signatureBytes) | ForEach-Object { $_.ToString('x2') })
    } finally {
        $signatureHasher.Dispose()
    }
}

function Reset-QuickRustBuildIfInputsChanged {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$CargoCommand,

        [Parameter(Mandatory)]
        [string]$ProjectRoot,

        [Parameter(Mandatory)]
        [string]$TargetRoot
    )

    $signature = Get-QuickRustSignature -ProjectRoot $ProjectRoot
    $signatureFile = Join-Path $ProjectRoot '.tools\build-cache\rust-quick.sha256'
    $previousSignature = if (Test-Path -LiteralPath $signatureFile) { (Get-Content -LiteralPath $signatureFile -Raw).Trim() } else { '' }
    if ($signature -eq $previousSignature) {
        Write-Host 'Rust inputs unchanged; reusing quick build cache.'
        return $signature
    }

    Write-Host 'Rust inputs changed; retaining the incremental quick build cache.'
    return $signature
}

function Save-QuickRustSignature {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$ProjectRoot,

        [Parameter(Mandatory)]
        [string]$Signature
    )

    $cacheDir = Join-Path $ProjectRoot '.tools\build-cache'
    New-Item -ItemType Directory -Force -Path $cacheDir | Out-Null
    Set-Content -LiteralPath (Join-Path $cacheDir 'rust-quick.sha256') -Value $Signature -NoNewline
}

function Invoke-FrontendBuild {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$PnpmCommand,

        [Parameter(Mandatory)]
        [string]$ProjectRoot
    )

    & $PnpmCommand run build
    if ($LASTEXITCODE -ne 0) {
        throw "Frontend production build failed with exit code $LASTEXITCODE."
    }
    $signature = Get-FrontendSignature -ProjectRoot $ProjectRoot
    Save-FrontendSignature -ProjectRoot $ProjectRoot -Signature $signature
    return $signature
}

function Ensure-FrontendBuild {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$PnpmCommand,

        [Parameter(Mandatory)]
        [string]$ProjectRoot,

        [switch]$Force
    )

    $currentSignature = Get-FrontendSignature -ProjectRoot $ProjectRoot
    $signatureFile = Join-Path $ProjectRoot '.tools\build-cache\frontend.sha256'
    $previousSignature = if (Test-Path -LiteralPath $signatureFile) { (Get-Content -LiteralPath $signatureFile -Raw).Trim() } else { '' }
    $distDir = Join-Path $ProjectRoot 'dist'

    $needsFrontendBuild = $Force -or -not (Test-Path -LiteralPath $distDir)
    if (-not $needsFrontendBuild) {
        $needsFrontendBuild = [string]::CompareOrdinal([string]$currentSignature, [string]$previousSignature) -ne 0
    }

    if ($needsFrontendBuild) {
        if ($Force) {
            Write-Host 'Release build: rebuilding production frontend.'
        } else {
            Write-Host 'Frontend inputs changed: rebuilding production frontend.'
        }
        $null = Invoke-FrontendBuild -PnpmCommand $PnpmCommand -ProjectRoot $ProjectRoot
    } else {
        Write-Host 'Frontend unchanged; reusing dist for quick build.'
    }

    return $currentSignature
}

function Invoke-MsvcBuild {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$VcvVars,

        [Parameter(Mandatory)]
        [string]$CargoCommand,

        [Parameter(Mandatory)]
        [string]$PnpmCommand,

        [Parameter(Mandatory)]
        [string]$TargetRoot,

        [Parameter(Mandatory)]
        [ValidateSet('quick', 'release')]
        [string]$Profile,

        [Parameter(Mandatory)]
        [string]$FrontendSignature,

        [switch]$Installer
    )

    $incremental = if ($Profile -eq 'quick') { '1' } else { '0' }
    if ($Installer) {
        $command = 'call "{0}" >nul && set "CARGO_TARGET_DIR={1}" && set "CARGO_INCREMENTAL={2}" && set "VERSADOWN_FRONTEND_SIGNATURE={3}" && call "{4}" exec tauri build --bundles nsis --config .tools/video-bundle.config.json --runner "{5}"' -f $VcvVars, $TargetRoot, $incremental, $FrontendSignature, $PnpmCommand, $CargoCommand
    } elseif ($Profile -eq 'quick') {
        $manifest = Join-Path $projectRoot 'src-tauri\Cargo.toml'
        $command = 'call "{0}" >nul && set "CARGO_TARGET_DIR={1}" && set "CARGO_INCREMENTAL={2}" && set "VERSADOWN_FRONTEND_SIGNATURE={3}" && "{4}" build --manifest-path "{5}" --profile quick --features custom-protocol,gui-subsystem' -f $VcvVars, $TargetRoot, $incremental, $FrontendSignature, $CargoCommand, $manifest
    } else {
        $manifest = Join-Path $projectRoot 'src-tauri\Cargo.toml'
        $command = 'call "{0}" >nul && set "CARGO_TARGET_DIR={1}" && set "CARGO_INCREMENTAL={2}" && set "VERSADOWN_FRONTEND_SIGNATURE={3}" && "{4}" build --manifest-path "{5}" --release --features custom-protocol' -f $VcvVars, $TargetRoot, $incremental, $FrontendSignature, $CargoCommand, $manifest
    }

    # Start-Process -Wait follows the whole descendant process tree on Windows.
    # Cargo/linker helpers can outlive the completed build command, which keeps
    # the script from ever reaching publish. A direct native invocation waits
    # for cmd.exe itself and preserves its exit code without following detached
    # descendants.
    & cmd.exe /d /s /c $command
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        throw "Windows $Profile build failed with exit code $exitCode."
    }
}

function Assert-PublishedBinaryAvailable {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$PublishedBinary,

        [object[]]$Processes = @(Get-Process -Name 'versadown' -ErrorAction SilentlyContinue)
    )

    $destination = [System.IO.Path]::GetFullPath($PublishedBinary)
    foreach ($existingProcess in $Processes) {
        $processPath = try { [System.IO.Path]::GetFullPath([string]$existingProcess.Path) } catch { continue }
        if ([string]::Equals($processPath, $destination, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "The published executable is running (PID $($existingProcess.Id)): $destination. Close it before starting the build."
        }
    }
}

function Publish-BuildOutput {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$BuiltBinary,

        [Parameter(Mandatory)]
        [string]$PublishedBinary,

        [Parameter(Mandatory)]
        [string]$SidecarSource,

        [Parameter(Mandatory)]
        [string]$PublishedSidecar,

        [Parameter(Mandatory)]
        [string]$RetiredXunleiHelper,

        [Parameter(Mandatory)]
        [string]$LegacyPublishedSidecar
    )

    foreach ($publishInput in @($BuiltBinary, $SidecarSource)) {
        if (-not (Test-Path -LiteralPath $publishInput -PathType Leaf)) {
            throw "Publish input was not found: $publishInput"
        }
    }

    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $PublishedBinary) | Out-Null
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $PublishedSidecar) | Out-Null
    $builtBinaryHash = Get-FileSha256 -Path $BuiltBinary
    Copy-Item -LiteralPath $BuiltBinary -Destination $PublishedBinary -Force
    Copy-Item -LiteralPath $SidecarSource -Destination $PublishedSidecar -Force
    $publishedBinaryHash = Get-FileSha256 -Path $PublishedBinary
    if ($publishedBinaryHash -ne $builtBinaryHash) {
        throw "Published executable hash did not match the build output: $PublishedBinary"
    }
    if (Test-Path -LiteralPath $RetiredXunleiHelper -PathType Leaf) {
        Remove-Item -LiteralPath $RetiredXunleiHelper -Force
    }
    if (Test-Path -LiteralPath $LegacyPublishedSidecar -PathType Leaf) {
        Remove-Item -LiteralPath $LegacyPublishedSidecar -Force
    }

    Write-Host "Published ($Mode): $PublishedBinary (SHA-256: $publishedBinaryHash)"
    Write-Host "Published ($Mode): $PublishedSidecar"
}

if ($DryRun) {
    $buildKind = if ($Installer) { 'release installer package' } else { "$profile executable" }
    Write-Host "Dry run: $buildKind"
    Write-Host "Cargo target: $targetRoot"
    Write-Host "Published executable: $publishedBinary"
    if ($Mode -eq 'Quick') {
        Write-Host 'Frontend: rebuild only when its source signature changes.'
        Write-Host 'Rust: recompile the versadown package when its source signature changes.'
    } elseif ($Installer) {
        Write-Host 'Frontend: Tauri executes the production beforeBuildCommand.'
    } else {
        Write-Host 'Frontend: always run the production build.'
    }
    exit 0
}

$buildTools = Get-VersaDownBuildTools -ProjectRoot $projectRoot
$pnpmCommand = $buildTools.Pnpm
$cargoCommand = $buildTools.Cargo
$vcvars = Get-VersaDownMsvcEnvironment
$env:Path = "$(Split-Path -Parent $pnpmCommand);$(Split-Path -Parent $cargoCommand);$env:Path"

Set-Location $projectRoot
Assert-PublishedBinaryAvailable -PublishedBinary $publishedBinary
$quickRustSignature = $null
if ($Installer) {
    & node scripts/prepare-video-tools.mjs
    if ($LASTEXITCODE -ne 0) { throw "Preparing video tools failed with exit code $LASTEXITCODE." }
    # Tauri owns beforeBuildCommand for a package build, so it performs the
    # frontend production build exactly once.
    $frontendSignature = Get-FrontendSignature -ProjectRoot $projectRoot
    Invoke-MsvcBuild -VcvVars $vcvars -CargoCommand $cargoCommand -PnpmCommand $pnpmCommand -TargetRoot $targetRoot -Profile $profile -FrontendSignature $frontendSignature -Installer
    Save-FrontendSignature -ProjectRoot $projectRoot -Signature $frontendSignature
} else {
    $frontendSignature = Ensure-FrontendBuild -PnpmCommand $pnpmCommand -ProjectRoot $projectRoot -Force:($Mode -eq 'Release')
    if ($Mode -eq 'Quick') {
        $quickRustSignature = Reset-QuickRustBuildIfInputsChanged -CargoCommand $cargoCommand -ProjectRoot $projectRoot -TargetRoot $targetRoot
    }
    Invoke-MsvcBuild -VcvVars $vcvars -CargoCommand $cargoCommand -PnpmCommand $pnpmCommand -TargetRoot $targetRoot -Profile $profile -FrontendSignature $frontendSignature
    if ($null -ne $quickRustSignature) {
        # Persist the post-build state rather than the pre-build snapshot. This
        # prevents a stale signature from making a later quick build reuse an
        # executable compiled from older Rust sources.
        Save-QuickRustSignature -ProjectRoot $projectRoot -Signature (Get-QuickRustSignature -ProjectRoot $projectRoot)
    }
}

$builtBinary = Join-Path $targetRoot "$profile\versadown.exe"
Publish-BuildOutput -BuiltBinary $builtBinary -PublishedBinary $publishedBinary -SidecarSource $sidecarSource -PublishedSidecar $publishedSidecar -RetiredXunleiHelper $retiredXunleiHelper -LegacyPublishedSidecar $legacyPublishedSidecar
