#!/usr/bin/env pwsh
<#
.SYNOPSIS
    Build the release binary and package it, with the docs, into a zip.

.DESCRIPTION
    Works with both Windows PowerShell 5.1 and PowerShell 7+.

    Runs the quality gate (fmt, clippy, tests), builds `cargo build --release`,
    stages the binary next to README.md and LICENSE, and writes a
    `sshftp-tui-<version>-<triple>.zip` (containing a single
    `sshftp-tui-<version>-<triple>/` folder) into `target/dist`.

    The version comes from the [package] section of Cargo.toml and the
    triple from `rustc -vV`, so the archive name always matches the build.

.EXAMPLE
    pwsh -File scripts/release.ps1

.EXAMPLE
    # Windows PowerShell 5.1
    powershell -ExecutionPolicy Bypass -File scripts/release.ps1

.EXAMPLE
    # Package a cross-compiled target without re-running the gate.
    powershell -ExecutionPolicy Bypass -File scripts/release.ps1 `
        -Target x86_64-unknown-linux-musl -SkipChecks

.EXAMPLE
    # Re-package the binary that is already in target/release.
    powershell -ExecutionPolicy Bypass -File scripts/release.ps1 -SkipBuild -SkipChecks
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [string]$Version,

    [string]$Target,

    [string]$OutputDir,

    [switch]$SkipBuild,

    [switch]$SkipChecks
)

$ErrorActionPreference = 'Stop'

$Root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$Name = 'sshftp-tui'
if (-not $OutputDir) { $OutputDir = Join-Path $Root 'target\dist' }

$script:TempRoot = $null

<#
    Windows PowerShell 5.1 turns native stderr into a terminating error while
    $ErrorActionPreference is 'Stop', and cargo is happy to write progress and
    warnings to stderr, so relax it for the duration of each call.
#>
function Invoke-Stream {
    param([string]$Command, [string[]]$Arguments)
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        # Folding stderr into the host stream stops Windows PowerShell 5.1 from
        # rendering cargo's progress lines as NativeCommandError blocks.
        & $Command @Arguments 2>&1 | ForEach-Object { Write-Host $_ }
    } finally {
        $ErrorActionPreference = $previous
    }
}

function Get-CommandOutput {
    param([string]$Command, [string[]]$Arguments)
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        return (& $Command @Arguments 2>&1 | Out-String)
    } finally {
        $ErrorActionPreference = $previous
    }
}

function Invoke-Cargo {
    param([string[]]$Arguments, [string]$What)
    Invoke-Stream cargo $Arguments
    if ($LASTEXITCODE -ne 0) { throw "$What failed (exit $LASTEXITCODE)." }
}

<#
    Read `version` from the [package] section only: the dependency tables are
    full of `version = "..."` lines that must not win.
#>
function Get-CrateVersion {
    $inPackage = $false
    foreach ($line in Get-Content (Join-Path $Root 'Cargo.toml')) {
        if ($line -match '^\s*\[') {
            $inPackage = $line -match '^\s*\[package\]'
            continue
        }
        if ($inPackage -and $line -match '^\s*version\s*=\s*"([^"]+)"') {
            return $Matches[1]
        }
    }
    throw 'Could not find the [package] version in Cargo.toml.'
}

<#
    `rustc -vV` reports the exact host triple, which is what cargo uses for an
    untargeted build. Fall back to a rough description if rustc is unavailable.
#>
function Get-HostTriple {
    try {
        $output = Get-CommandOutput rustc @('-vV')
        if ($output -match '(?m)^host:\s*(\S+)') { return $Matches[1] }
    } catch { }
    $arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } elseif ([Environment]::Is64BitOperatingSystem) { 'x86_64' } else { 'i686' }
    return "$arch-unknown"
}

function Format-Size {
    param([long]$Bytes)
    $units = 'B', 'KiB', 'MiB', 'GiB'
    $value = [double]$Bytes
    $index = 0
    while ($value -ge 1024 -and $index -lt $units.Count - 1) {
        $value /= 1024
        $index++
    }
    if ($index -eq 0) { return "$Bytes B" }
    return ('{0:N1} {1}' -f $value, $units[$index])
}

Push-Location $Root
try {
    if (-not $Version) { $Version = Get-CrateVersion }
    $triple = if ($Target) { $Target } else { Get-HostTriple }

    $releaseDir = if ($Target) {
        Join-Path $Root (Join-Path 'target' (Join-Path $Target 'release'))
    } else {
        Join-Path $Root 'target\release'
    }
    $extension = if ($triple -match 'windows') { '.exe' } else { '' }
    $binary = Join-Path $releaseDir ($Name + $extension)

    $packageName = "$Name-$Version-$triple"
    $zipPath = Join-Path $OutputDir "$packageName.zip"

    Write-Host "$Name $Version  ($triple)"

    if (-not $SkipChecks) {
        Invoke-Cargo @('fmt', '--check') 'cargo fmt --check'
        Invoke-Cargo @('clippy', '--all-targets', '--', '-D', 'warnings') 'cargo clippy'
        Invoke-Cargo @('test') 'cargo test'
    }

    if (-not $SkipBuild) {
        $buildArgs = @('build', '--release')
        if ($Target) { $buildArgs += @('--target', $Target) }
        Invoke-Cargo $buildArgs 'cargo build --release'
    }

    if (-not (Test-Path -LiteralPath $binary)) {
        throw "Release binary not found at $binary. Build it first (drop -SkipBuild)."
    }

    # Stage the payload under its final folder name, then zip the temp root so
    # the archive contains exactly that one folder.
    $script:TempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("$Name-pack-" + [Guid]::NewGuid().ToString('N'))
    $packageDir = Join-Path $script:TempRoot $packageName
    New-Item -ItemType Directory -Path $packageDir -Force | Out-Null

    Copy-Item -LiteralPath $binary -Destination $packageDir
    foreach ($file in 'README.md', 'LICENSE') {
        $source = Join-Path $Root $file
        if (Test-Path -LiteralPath $source) {
            Copy-Item -LiteralPath $source -Destination $packageDir
        } else {
            Write-Warning "Skipping $file (not found)."
        }
    }

    if (-not (Test-Path -LiteralPath $OutputDir)) {
        New-Item -ItemType Directory -Path $OutputDir -Force | Out-Null
    }
    if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }

    Add-Type -AssemblyName System.IO.Compression.FileSystem -ErrorAction SilentlyContinue
    [System.IO.Compression.ZipFile]::CreateFromDirectory(
        $script:TempRoot,
        $zipPath,
        [System.IO.Compression.CompressionLevel]::Optimal,
        $false
    )

    Write-Host 'Packaged:'
    $archive = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
    try {
        foreach ($entry in $archive.Entries) {
            Write-Host ('  {0,-52} {1}' -f $entry.FullName, (Format-Size $entry.Length))
        }
    } finally {
        $archive.Dispose()
    }

    $size = (Get-Item -LiteralPath $zipPath).Length
    Write-Host ''
    Write-Host ('Wrote {0} ({1})' -f $zipPath, (Format-Size $size))
} finally {
    Pop-Location
    if ($script:TempRoot -and (Test-Path -LiteralPath $script:TempRoot)) {
        Remove-Item -LiteralPath $script:TempRoot -Recurse -Force
    }
}
