param(
    [string]$Version = "",
    [string]$Configuration = "release"
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($Version)) {
    $manifest = Get-Content (Join-Path $Root "Cargo.toml")
    $versionPattern = '^version\s*=\s*"([^"]+)"'
    $versionLine = $manifest | Where-Object { $_ -match $versionPattern } | Select-Object -First 1
    if (-not $versionLine) {
        throw "Unable to read version from Cargo.toml"
    }
    $Version = [regex]::Match($versionLine, $versionPattern).Groups[1].Value
}
$Exe = Join-Path $Root "target\$Configuration\screen-translator.exe"
$Runtime = Join-Path $Root "onnxruntime.dll"
$OutputRoot = Join-Path $Root "dist"
$PackageName = "ScreenTranslator-v$Version-win64"
$Stage = Join-Path $OutputRoot $PackageName

if (-not (Test-Path $Exe)) {
    throw "Missing executable: $Exe. Run cargo build --release first."
}
if (-not (Test-Path $Runtime)) {
    throw "Missing onnxruntime.dll"
}

Remove-Item $Stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Stage | Out-Null
Copy-Item $Exe (Join-Path $Stage "ScreenTranslator.exe")
Copy-Item $Runtime $Stage
Copy-Item (Join-Path $Root "assets") $Stage -Recurse
Copy-Item (Join-Path $Root "README.md") $Stage
Copy-Item (Join-Path $Root "LICENSE") $Stage
Copy-Item (Join-Path $Root "docs\licenses.md") (Join-Path $Stage "licenses.md")
Copy-Item (Join-Path $Root "docs\privacy.md") $Stage
Copy-Item (Join-Path $Root "docs") $Stage -Recurse
if (Test-Path (Join-Path $Root "licenses")) {
    Copy-Item (Join-Path $Root "licenses") $Stage -Recurse
}

$Zip = Join-Path $OutputRoot "$PackageName.zip"
Remove-Item $Zip -Force -ErrorAction SilentlyContinue
Compress-Archive -Path "$Stage\*" -DestinationPath $Zip -CompressionLevel Optimal

$SizeMb = (Get-Item $Zip).Length / 1MB
if ($SizeMb -gt 30) {
    throw "Package size $([math]::Round($SizeMb, 2)) MB exceeds the 30 MB budget"
}
Write-Host "Created $Zip ($([math]::Round($SizeMb, 2)) MB)"
