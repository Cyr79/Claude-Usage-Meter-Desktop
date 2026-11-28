# Builds an UNSIGNED .msix for Microsoft Store upload (the Store signs it).
# Run after `npm run tauri build`. Fill the PLACEHOLDER_* values in AppxManifest.xml first.
$ErrorActionPreference = 'Stop'
$root = Resolve-Path "$PSScriptRoot\..\.."
$makeappx = 'C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\makeappx.exe'
$exe = Join-Path $root 'src-tauri\target\release\claude-usage-meter.exe'
$icons = Join-Path $root 'src-tauri\icons'
$out = Join-Path $root 'packaging\msix\out'
$stage = Join-Path $out 'stage'

if (-not (Test-Path $makeappx)) { throw "makeappx.exe not found: $makeappx" }
if (-not (Test-Path $exe)) { throw "Release exe missing, run 'npm run tauri build' first: $exe" }
if (Select-String -Path "$PSScriptRoot\AppxManifest.xml" -Pattern 'PLACEHOLDER_' -Quiet) {
  Write-Warning 'AppxManifest.xml still has PLACEHOLDER_ values: fine for a local test pack, not for Store upload.'
}

if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force "$stage\Assets" | Out-Null
Copy-Item $exe $stage
Copy-Item "$PSScriptRoot\AppxManifest.xml" $stage
foreach ($f in 'StoreLogo.png', 'Square150x150Logo.png', 'Square44x44Logo.png') {
  Copy-Item (Join-Path $icons $f) "$stage\Assets\$f"
}

$version = ([xml](Get-Content "$PSScriptRoot\AppxManifest.xml")).Package.Identity.Version
$msix = Join-Path $out "ClaudeUsageMeter_${version}_x64.msix"
& $makeappx pack /o /d $stage /p $msix
if ($LASTEXITCODE -ne 0) { throw "makeappx failed ($LASTEXITCODE)" }
Write-Host "Unsigned MSIX: $msix"
