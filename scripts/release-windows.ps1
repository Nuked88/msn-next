param(
    [ValidateSet('major','minor','patch','keep','')]
    [string]$Bump = '',
    [switch]$SkipInstall,
    [switch]$Msi
)

$ErrorActionPreference = 'Stop'
$desktop = Join-Path (Split-Path $PSScriptRoot -Parent) 'apps\desktop'

Push-Location $desktop
try {
    if (-not $SkipInstall) {
        npm ci
        if ($LASTEXITCODE) { throw "npm ci non riuscito" }
    }
    $bumpScript = Join-Path (Split-Path $PSScriptRoot -Parent) 'scripts\bump-version.mjs'
    if ($Bump) { node $bumpScript $Bump } else { node $bumpScript }
    if ($LASTEXITCODE) { throw "bump versione non riuscito" }
    npm run check
    if ($LASTEXITCODE) { throw "controlli non riusciti" }
    npm run release:windows
    if ($LASTEXITCODE) { throw "creazione release non riuscita" }
    if ($Msi) {
        npm run release:windows:msi
        if ($LASTEXITCODE) { throw "creazione MSI non riuscita" }
    }
} finally {
    Pop-Location
}

Write-Host "Release pronta in target\release e setup in target\release\bundle\nsis"
