#Requires -RunAsAdministrator
<#
    Registers, starts, stops or removes the Virtual KM Switch service.

    The service must run as LocalSystem. Nothing else can create objects in the Global namespace,
    place a process inside another session, or touch the Winlogon desktop -- and all three are
    required for the agents to work at all.

    Usage:
        .\service.ps1 install    register the release build and start it
        .\service.ps1 start
        .\service.ps1 stop
        .\service.ps1 status
        .\service.ps1 uninstall
        .\service.ps1 logs       tail the service and agent logs
#>

[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet('install', 'start', 'stop', 'status', 'uninstall', 'logs')]
    [string]$Action = 'status'
)

$ErrorActionPreference = 'Stop'

$serviceName  = 'VirtualKmSwitch'
$firewallName = 'Virtual KM Switch'
$root         = Split-Path -Parent $PSScriptRoot
$exePath      = Join-Path $root 'src\rust\target\release\vkms-service.exe'
$agentPath    = Join-Path $root 'src\rust\target\release\vkms-agent.exe'
$agentProcess = 'vkms-agent'
$logDir       = Join-Path $env:ProgramData 'VirtualKmSwitch'

function Get-VirtualKmSwitchService {
    Get-Service -Name $serviceName -ErrorAction SilentlyContinue
}

function Stop-Agents {
    # Agents are stopped by the service, but a service that crashed outright would leave them
    # behind, still hooking input.
    Get-Process -Name $agentProcess -ErrorAction SilentlyContinue |
        Stop-Process -Force -ErrorAction SilentlyContinue
}

function Show-Status {
    $service = Get-VirtualKmSwitchService
    if (-not $service) {
        Write-Host "$serviceName is not installed." -ForegroundColor Yellow
        return
    }

    Write-Host "$serviceName : $($service.Status)" -ForegroundColor Cyan

    $agents = Get-Process -Name $agentProcess -ErrorAction SilentlyContinue
    if ($agents) {
        Write-Host "agents running: $($agents.Count)" -ForegroundColor Green
        $agents | ForEach-Object { Write-Host "  pid $($_.Id)" }
    }
    else {
        Write-Host 'agents running: none' -ForegroundColor Yellow
    }
}

switch ($Action) {
    'install' {
        if (-not (Test-Path $exePath) -or -not (Test-Path $agentPath)) {
            Write-Host 'Not built. Run: cargo build --release, in src\rust' -ForegroundColor Red
            Write-Host "Expected: $exePath"
            Write-Host "     and: $agentPath"
            exit 1
        }

        if (Get-VirtualKmSwitchService) {
            Write-Host 'Already installed; removing first...'
            & sc.exe stop $serviceName | Out-Null
            Start-Sleep -Seconds 2
            & sc.exe delete $serviceName | Out-Null
            Start-Sleep -Seconds 1
            Stop-Agents
        }

        Write-Host "Registering $serviceName as LocalSystem..." -NoNewline

        # binPath needs the executable quoted: the path contains spaces on many machines, and
        # sc.exe would otherwise treat the remainder as arguments.
        $result = & sc.exe create $serviceName binPath= "`"$exePath`"" start= auto obj= LocalSystem DisplayName= 'Virtual KM Switch Input Service'
        if ($LASTEXITCODE -ne 0) {
            Write-Host ''
            Write-Host $result -ForegroundColor Red
            exit 1
        }

        & sc.exe description $serviceName 'Shares one keyboard and mouse across paired machines on the LAN.' | Out-Null
        Write-Host ' done'

        # Inbound for the service executable only, and only on private and domain networks: the
        # mesh (TCP 42100), pairing (TCP 42101) and discovery (UDP 42099) all live in that process.
        Remove-NetFirewallRule -DisplayName $firewallName -ErrorAction SilentlyContinue
        New-NetFirewallRule -DisplayName $firewallName -Direction Inbound -Action Allow `
            -Program $exePath -Profile Private, Domain | Out-Null
        Write-Host 'Firewall rule added (private and domain networks).'

        Write-Host 'Starting...' -NoNewline
        Start-Service -Name $serviceName
        Write-Host ' done'
        Start-Sleep -Seconds 3
        Show-Status
    }

    'start' { Start-Service -Name $serviceName; Show-Status }

    'stop' {
        Stop-Service -Name $serviceName -Force -ErrorAction SilentlyContinue
        Stop-Agents
        Show-Status
    }

    'uninstall' {
        if (Get-VirtualKmSwitchService) {
            Stop-Service -Name $serviceName -Force -ErrorAction SilentlyContinue
            Start-Sleep -Seconds 2
            & sc.exe delete $serviceName | Out-Null
            Write-Host "$serviceName removed." -ForegroundColor Green
        }
        else {
            Write-Host 'Not installed.' -ForegroundColor Yellow
        }

        Remove-NetFirewallRule -DisplayName $firewallName -ErrorAction SilentlyContinue
        Stop-Agents
    }

    'logs' {
        Get-ChildItem -Path $logDir -Filter '*.log' -ErrorAction SilentlyContinue | ForEach-Object {
            Write-Host ''
            Write-Host "=== $($_.Name) ===" -ForegroundColor Cyan
            Get-Content $_.FullName -Tail 40
        }
    }

    default { Show-Status }
}
