#!/usr/bin/env pwsh
<#
.SYNOPSIS
    Manage the containerised OpenSSH server used by the end-to-end tests.

.DESCRIPTION
    Works with both Windows PowerShell 5.1 and PowerShell 7+.

    The `test` action starts the container, works out the address the test
    client should use, and runs `cargo test --test sshd_e2e -- --ignored`.

.EXAMPLE
    pwsh -File scripts/test-server.ps1 build
    pwsh -File scripts/test-server.ps1 test
    pwsh -File scripts/test-server.ps1 stop

    # Windows PowerShell 5.1
    powershell -ExecutionPolicy Bypass -File scripts/test-server.ps1 test
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet('build', 'start', 'stop', 'restart', 'reset-hosts', 'logs', 'fingerprint', 'status', 'test')]
    [string]$Action = 'start'
)

$ErrorActionPreference = 'Stop'

$Image   = 'sshftp-tui-test:latest'
$Name    = 'sshftp-tui-test'
$Volume  = 'sshftp-tui-hosts'
$Context = Join-Path $PSScriptRoot '..\tests\sshd'
$Port    = 2222

<#
    Windows PowerShell 5.1 turns native stderr into a terminating error while
    $ErrorActionPreference is 'Stop', and `podman` is happy to write progress
    and log output to stderr, so relax it for the duration of each call.
#>
function Invoke-Stream {
    param([string]$Command, [string[]]$Arguments)
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        # Folding stderr into the host stream stops Windows PowerShell 5.1 from
        # rendering cargo's/podman's progress lines as NativeCommandError blocks.
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

function Get-ContainerNames {
    $output = Get-CommandOutput podman @('ps', '-a', '--format', '{{.Names}}')
    return @(($output -split "\r?\n") | ForEach-Object { $_.Trim() } | Where-Object { $_ })
}

function Test-ContainerRunning {
    return (Get-ContainerNames) -contains $Name
}

function Test-Port {
    param([string]$Target, [int]$Port, [int]$TimeoutMs = 800)

    $client = New-Object System.Net.Sockets.TcpClient
    try {
        $connect = $client.BeginConnect($Target, $Port, $null, $null)
        if (-not $connect.AsyncWaitHandle.WaitOne($TimeoutMs)) { return $false }
        $client.EndConnect($connect)
        return $true
    } catch {
        return $false
    } finally {
        $client.Close()
    }
}

$script:MachineIp = $null
$script:MachineIpChecked = $false

<#
    Ask the machine which source address it would use to reach the outside: that
    is the address the host can talk to. Parsing `ip addr` instead is fragile
    because the loopback address of the WSL machine is a global one too, and it
    is listed first.
#>
function Get-MachineIp {
    if ($script:MachineIpChecked) { return $script:MachineIp }

    $route = Get-CommandOutput podman @('machine', 'ssh', 'ip route get 1.1.1.1')
    if ($route -match 'src (\d+\.\d+\.\d+\.\d+)') {
        $script:MachineIp = $Matches[1]
    } else {
        $fallback = Get-CommandOutput podman @('machine', 'ssh', 'ip route')
        if ($fallback -match 'src (\d+\.\d+\.\d+\.\d+)') {
            $script:MachineIp = $Matches[1]
        }
    }

    $script:MachineIpChecked = $true
    return $script:MachineIp
}

<#
    On Linux and macOS podman publishes the port on localhost. On Windows it runs
    inside a WSL machine where localhost forwarding is not set up, so the client
    has to use that machine's own address instead.
#>
function Get-Endpoint {
    for ($attempt = 0; $attempt -lt 30; $attempt++) {
        if (Test-Port -Target '127.0.0.1' -Port $Port) {
            return @{ Host = '127.0.0.1'; Port = $Port }
        }
        $ip = Get-MachineIp
        if ($ip -and (Test-Port -Target $ip -Port $Port)) {
            return @{ Host = $ip; Port = $Port }
        }
        Start-Sleep -Milliseconds 500
    }
    throw "Nothing is listening on port $Port yet; check 'podman logs $Name'."
}

function Invoke-Build {
    Invoke-Stream podman @('build', '-t', $Image, $Context)
    if ($LASTEXITCODE -ne 0) { throw "podman build failed (exit $LASTEXITCODE)" }
}

function Invoke-Start {
    if (Test-ContainerRunning) {
        Write-Host "Container '$Name' is already running."
    } else {
        if ((Get-ContainerNames) -contains $Name) {
            Invoke-Stream podman @('rm', '-f', $Name)
        }
        Invoke-Stream podman @(
            'run', '--rm', '-d', '--name', $Name,
            '-p', "${Port}:2222",
            '-v', "${Volume}:/etc/ssh/keys",
            $Image
        )
        if ($LASTEXITCODE -ne 0) { throw "podman run failed (exit $LASTEXITCODE)" }
    }

    $endpoint = Get-Endpoint
    Invoke-Stream podman @('ps', '--filter', "name=$Name", '--format', '{{.Names}}  {{.Status}}  {{.Ports}}')
    Write-Host "SSH + SFTP ready on $($endpoint.Host):$($endpoint.Port)  (tester / testpass)"
}

function Invoke-Stop {
    if (Test-ContainerRunning) {
        Invoke-Stream podman @('rm', '-f', $Name)
    }
    Write-Host "Container '$Name' stopped."
}

function Invoke-ResetHosts {
    Invoke-Stop
    Invoke-Stream podman @('volume', 'rm', '-f', $Volume)
    Write-Host "Host-key volume '$Volume' removed; the next start generates fresh keys."
}

function Invoke-Fingerprint {
    Invoke-Stream podman @('exec', $Name, 'ssh-keygen', '-lf', '/etc/ssh/keys/ssh_host_ed25519_key.pub')
}

function Invoke-Logs {
    Invoke-Stream podman @('logs', '--tail', '200', $Name)
}

switch ($Action) {
    'build'       { Invoke-Build }
    'start'       { Invoke-Start }
    'stop'        { Invoke-Stop }
    'restart'     { Invoke-Stop; Invoke-Start }
    'reset-hosts' { Invoke-ResetHosts }
    'logs'        { Invoke-Logs }
    'fingerprint' { Invoke-Fingerprint }
    'status'      { Invoke-Stream podman @('ps', '-a', '--filter', "name=$Name") }
    'test' {
        Invoke-Start
        $endpoint = Get-Endpoint
        $env:SSHFTP_TUI_TEST_HOST = $endpoint.Host
        $env:SSHFTP_TUI_TEST_PORT = "$($endpoint.Port)"
        Write-Host "Running the end-to-end suite against $($endpoint.Host):$($endpoint.Port)"
        Invoke-Stream cargo @('test', '--test', 'sshd_e2e', '--', '--ignored', '--nocapture')
        if ($LASTEXITCODE -ne 0) { throw "the end-to-end suite failed (exit $LASTEXITCODE)" }
    }
}
