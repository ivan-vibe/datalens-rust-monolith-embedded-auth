# Windows wrapper: builds and starts the cluster from PowerShell.
#
# Requirements: Docker Desktop for Windows with the WSL2 backend, and a WSL2 distro
# (Ubuntu is installed by default by `wsl --install`). The stack itself runs as Linux
# containers, so the same images/binaries work on Windows.
#
#   .\deploy\cluster.ps1            # = up
#   .\deploy\cluster.ps1 down
#   .\deploy\cluster.ps1 reset
param(
    [Parameter(Position = 0)]
    [ValidateSet('up', 'down', 'reset', 'ps', 'logs', 'restart', 'stop', 'start', 'build', 'config')]
    [string]$Command = 'up'
)

$ErrorActionPreference = 'Stop'

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    throw 'Docker Desktop is not installed. Install it with the WSL2 backend: https://docs.docker.com/desktop/windows/'
}
docker info *> $null
if ($LASTEXITCODE -ne 0) {
    throw 'The Docker daemon is not running. Start Docker Desktop first.'
}
if (-not (Get-Command wsl -ErrorAction SilentlyContinue)) {
    throw 'WSL2 is not installed. Run `wsl --install -d Ubuntu` in an elevated PowerShell, then retry.'
}

$root = Split-Path -Parent $PSScriptRoot
$wslRoot = (wsl wslpath -a "$root").Trim()
$extra = if ($args.Count -gt 0) { ($args -join ' ') } else { '' }
$line = "cd '$wslRoot' && ./deploy/cluster.sh $Command $extra"
wsl -e bash -lc $line
exit $LASTEXITCODE
