<#
    Builds the Virtual KM Switch installer: target\installer\VirtualKmSwitch-<version>-x64-setup.exe, and the
    MSI inside it, target\installer\VirtualKmSwitch-<version>-x64.msi.

    Release-builds the Rust service and agent, publishes the controller self-contained (so the
    machine it is installed on needs no .NET runtime), then packages all three with WiX: the MSI,
    then the setup bundle that stops the service before running it. The version is the workspace's,
    from Cargo.toml.

    Needs what an ordinary build needs (Rust with the MSVC toolchain, the .NET SDK); WiX comes from
    NuGet. No elevation: building the MSI changes nothing on this machine.

    Usage:
        .\build.ps1
#>

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$root          = Split-Path -Parent $PSScriptRoot
$controllerDir = Join-Path $root 'target\installer\controller'

function Invoke-Step([string]$name, [scriptblock]$command) {
    Write-Host "== $name" -ForegroundColor Cyan
    & $command
    if ($LASTEXITCODE -ne 0) {
        Write-Host "$name failed (exit $LASTEXITCODE)." -ForegroundColor Red
        exit $LASTEXITCODE
    }
}

# The first version = "..." in Cargo.toml is [workspace.package]'s, which every crate inherits.
$cargoToml = Get-Content (Join-Path $root 'src\rust\Cargo.toml') -Raw
if ($cargoToml -notmatch '(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"') {
    Write-Host 'No version found in Cargo.toml.' -ForegroundColor Red
    exit 1
}
$version = $Matches[1]
Write-Host "Virtual KM Switch $version"

Invoke-Step 'Service and agent' {
    cargo build --release --manifest-path (Join-Path $root 'src\rust\Cargo.toml') -p vkms-service -p vkms-agent
}

# Cleared first: a file left over from an earlier publish would otherwise be packaged too.
Remove-Item -Recurse -Force $controllerDir -ErrorAction SilentlyContinue

Invoke-Step 'Controller' {
    dotnet publish (Join-Path $root 'src\csharp\VirtualKmSwitch.Gui\VirtualKmSwitch.Gui.csproj') -c Release -r win-x64 `
        --self-contained -p:PublishSingleFile=true -p:DebugType=none -p:Version=$version -o $controllerDir
}

Invoke-Step 'Installer' {
    dotnet build (Join-Path $PSScriptRoot 'VirtualKmSwitch.Installer.wixproj') -c Release -p:ProductVersion=$version
}

Invoke-Step 'Setup bundle' {
    dotnet build (Join-Path $PSScriptRoot 'bundle\VirtualKmSwitch.Bundle.wixproj') -c Release -p:ProductVersion=$version
}

$setup = Join-Path $root "target\installer\VirtualKmSwitch-$version-x64-setup.exe"
Write-Host ''
Write-Host "Built $setup" -ForegroundColor Green
