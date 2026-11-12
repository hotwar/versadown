$scriptPath = Join-Path $PSScriptRoot 'build-tools.ps1'
. $scriptPath

Describe 'Resolve-VersaDownBuildTool' {
    It 'skips a zero-byte Cargo placeholder and selects the usable Cargo executable' {
        $placeholder = Join-Path $TestDrive 'cargo-placeholder.exe'
        [System.IO.File]::WriteAllBytes($placeholder, [byte[]]@())
        $usableCargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'

        $resolved = Resolve-VersaDownBuildTool -ToolName 'Cargo' -Candidates @($placeholder, $usableCargo)

        $resolved | Should Be (Get-Item -LiteralPath $usableCargo).FullName
    }

    It 'selects the configured pnpm command when it can execute' {
        $pnpm = Join-Path $env:USERPROFILE '.cache\codex-runtimes\codex-primary-runtime\dependencies\bin\fallback\pnpm.cmd'

        $resolved = Resolve-VersaDownBuildTool -ToolName 'pnpm' -Candidates @($pnpm)

        $resolved | Should Be (Get-Item -LiteralPath $pnpm).FullName
    }

    It 'fails clearly when none of the supplied candidates are usable' {
        $missingTool = Join-Path $TestDrive 'missing-tool.exe'
        $didThrow = $false

        try {
            Resolve-VersaDownBuildTool -ToolName 'Cargo' -Candidates @($missingTool)
        } catch {
            $didThrow = $true
        }

        $didThrow | Should Be $true
    }
}

Describe 'Get-VersaDownBuildTools' {
    It 'prefers configured PATH pnpm when it is available' {
        $projectRoot = Split-Path -Parent $PSScriptRoot

        $tools = Get-VersaDownBuildTools -ProjectRoot $projectRoot

        $tools.Cargo | Should Be (Get-Item -LiteralPath (Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe')).FullName
        $pathPnpm = (Get-Command pnpm.cmd -ErrorAction SilentlyContinue).Source
        $expectedPnpm = if ($pathPnpm) { $pathPnpm } else { Join-Path $env:USERPROFILE '.cache\codex-runtimes\codex-primary-runtime\dependencies\bin\fallback\pnpm.cmd' }
        $tools.Pnpm | Should Be (Get-Item -LiteralPath $expectedPnpm).FullName
    }
}
