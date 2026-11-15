param([string]$Destination = "$PSScriptRoot\..\dist")
$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path -LiteralPath "$PSScriptRoot\..").Path
Push-Location -LiteralPath $projectRoot
try {
    cargo build --locked --offline --release --target x86_64-pc-windows-msvc --target-dir target -j 2
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
    $manifest = Get-Content -LiteralPath "$projectRoot\Cargo.toml" -Raw
    $version = [regex]::Match($manifest, '(?m)^version = "([^"]+)"').Groups[1].Value
    if (-not $version) { throw 'Package version not found.' }
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    $asset = Join-Path $Destination "DiskMigrator-v$version.exe"
    Copy-Item -LiteralPath "$projectRoot\target\x86_64-pc-windows-msvc\release\DiskMigrator.exe" -Destination $asset
    $checksum = (Get-FileHash -LiteralPath $asset -Algorithm SHA256).Hash.ToLowerInvariant()
    "$checksum  DiskMigrator-v$version.exe" | Set-Content -LiteralPath (Join-Path $Destination 'SHA256SUMS') -Encoding ascii
    Copy-Item -LiteralPath "$projectRoot\LICENSE" -Destination $Destination
    Copy-Item -LiteralPath "$projectRoot\assets\THIRD-PARTY-NOTICES.txt" -Destination $Destination
    Write-Output $asset
} finally { Pop-Location }
