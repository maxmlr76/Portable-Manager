# Writes the licenses of all Rust crates compiled into the app to one text file.
# MIT and similar licenses require their notices to travel with the binaries.
param([Parameter(Mandatory)][string]$OutFile)
$ErrorActionPreference = "Stop"

$root = Split-Path $PSScriptRoot -Parent
Push-Location $root
try {
    $used = cargo tree --locked -p maxtrix-app --target x86_64-pc-windows-msvc -e normal --prefix none --format "{p}" |
        ForEach-Object { ($_ -split " ")[0..1] -join " " } | Sort-Object -Unique
    $meta = cargo metadata --format-version 1 --locked | ConvertFrom-Json -Depth 30
} finally {
    Pop-Location
}

$sw = [Text.StringBuilder]::new()
[void]$sw.AppendLine("LICENSES OF THE RUST CRATES COMPILED INTO MAXTRIX PORTABLE MANAGER")
[void]$sw.AppendLine("")
$texts = [ordered]@{}
$count = 0
foreach ($pkg in ($meta.packages | Sort-Object name, version)) {
    if ("$($pkg.name) v$($pkg.version)" -notin $used) { continue }
    if ($pkg.name -like "maxtrix-*") { continue }
    $count++
    [void]$sw.AppendLine("$($pkg.name) $($pkg.version): $($pkg.license)")
    $dir = Split-Path $pkg.manifest_path
    $files = @(Get-ChildItem -LiteralPath $dir -File | Where-Object { $_.Name -match '^(LICEN[CS]E|COPYING|NOTICE|AUTHORS)' })
    $sub = Join-Path $dir "LICENSES"
    if (Test-Path -LiteralPath $sub) { $files += Get-ChildItem -LiteralPath $sub -File }
    foreach ($f in $files) {
        $text = (Get-Content -LiteralPath $f.FullName -Raw)
        if (-not $text) { continue }
        $key = $text.Trim()
        if (-not $texts.Contains($key)) { $texts[$key] = [Collections.Generic.List[string]]::new() }
        $texts[$key].Add("$($pkg.name) $($pkg.version) ($($f.Name))")
    }
}
foreach ($entry in $texts.GetEnumerator()) {
    [void]$sw.AppendLine("")
    [void]$sw.AppendLine(("=" * 78))
    [void]$sw.AppendLine("Used by: " + ($entry.Value -join ", "))
    [void]$sw.AppendLine(("=" * 78))
    [void]$sw.AppendLine($entry.Key)
}
[IO.File]::WriteAllText($OutFile, $sw.ToString(), [Text.UTF8Encoding]::new($false))
Write-Output "$count crates, $($texts.Count) distinct license texts -> $OutFile"
