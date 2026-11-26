function Resolve-VersaDownBuildTool {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$ToolName,

        [Parameter(Mandatory)]
        [string[]]$Candidates
    )

    foreach ($candidate in $Candidates) {
        if ([string]::IsNullOrWhiteSpace($candidate) -or -not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            continue
        }

        $file = Get-Item -LiteralPath $candidate
        $isLink = [bool]($file.Attributes -band [System.IO.FileAttributes]::ReparsePoint)
        if ($file.Length -le 0 -and -not $isLink) {
            continue
        }

        try {
            $probe = New-Object System.Diagnostics.Process
            if ($file.Extension -in @('.cmd', '.bat')) {
                $probe.StartInfo.FileName = $env:ComSpec
                $probe.StartInfo.Arguments = '/d /s /c ""{0}" --version"' -f $file.FullName
            } else {
                $probe.StartInfo.FileName = $file.FullName
                $probe.StartInfo.Arguments = '--version'
            }
            $probe.StartInfo.UseShellExecute = $false
            $probe.StartInfo.CreateNoWindow = $true
            if ($probe.Start()) {
                $probe.WaitForExit()
            }
            if ($probe.HasExited -and $probe.ExitCode -eq 0) {
                return $file.FullName
            }
        } catch {
            continue
        } finally {
            if ($probe) {
                $probe.Dispose()
            }
        }
    }

    throw "No usable $ToolName tool was found."
}

function Get-VersaDownBuildTools {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$ProjectRoot
    )

    # Sandboxed Windows shells may virtualize GetFolderPath while keeping the
    # real tool installation under USERPROFILE. Prefer the explicit process
    # environment so discovery and the build use the same account paths.
    $userProfile = $env:USERPROFILE
    if ([string]::IsNullOrWhiteSpace($userProfile)) {
        $userProfile = [Environment]::GetFolderPath('UserProfile')
    }
    $codexRuntime = Join-Path $userProfile '.cache\codex-runtimes\codex-primary-runtime\dependencies'
    $projectCargo = Join-Path $ProjectRoot '.tools\cargo\bin\cargo.exe'
    $projectPnpm = Join-Path $ProjectRoot '.tools\node-v24.20.0-win-x64\node_modules\corepack\shims\pnpm.cmd'
    $pathCargo = (Get-Command cargo.exe -ErrorAction SilentlyContinue).Source
    $pathPnpm = (Get-Command pnpm.cmd -ErrorAction SilentlyContinue).Source

    [pscustomobject]@{
        Cargo = Resolve-VersaDownBuildTool -ToolName 'Cargo' -Candidates @(
            (Join-Path $userProfile '.cargo\bin\cargo.exe'),
            $projectCargo,
            $pathCargo
        )
        Pnpm = Resolve-VersaDownBuildTool -ToolName 'pnpm' -Candidates @(
            $pathPnpm,
            (Join-Path $codexRuntime 'bin\fallback\pnpm.cmd'),
            $projectPnpm
        )
    }
}

function Get-VersaDownMsvcEnvironment {
    [CmdletBinding()]
    param()

    $candidates = @($env:VCVARS_PATH)
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path -LiteralPath $vswhere -PathType Leaf) {
        $installation = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if ($LASTEXITCODE -eq 0 -and -not [string]::IsNullOrWhiteSpace($installation)) {
            $candidates += Join-Path $installation 'VC\Auxiliary\Build\vcvars64.bat'
        }
    }

    foreach ($edition in @('BuildTools', 'Enterprise', 'Professional', 'Community')) {
        foreach ($programFiles in @($env:ProgramFiles, ${env:ProgramFiles(x86)})) {
            if (-not [string]::IsNullOrWhiteSpace($programFiles)) {
                $candidates += Join-Path $programFiles "Microsoft Visual Studio\2022\$edition\VC\Auxiliary\Build\vcvars64.bat"
            }
        }
    }

    foreach ($candidate in $candidates) {
        if (-not [string]::IsNullOrWhiteSpace($candidate) -and (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            return (Get-Item -LiteralPath $candidate).FullName
        }
    }
    throw 'Visual Studio C++ build environment was not found.'
}
