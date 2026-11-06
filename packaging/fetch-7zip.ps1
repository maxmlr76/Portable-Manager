# Fetches the official 7-Zip into vendor\7-zip (not stored in the repository).
# Version and checksums are pinned here: a changed download stops the build.
# To update: take the new version and its SHA-256 values from
# https://github.com/ip7z/7zip/releases
$ErrorActionPreference = "Stop"

$tag = "26.05"
$installer = "7z2605-x64.exe"
$installerSha = "fd99e86f57d8f429ef5ed045bf98315af962e2309b0f1ba87a147a44c71647d0"
$bootstrap = "7zr.exe"
$bootstrapSha = "9bf582b894dfa665f852d7e9112b02290a1c303c2d95a21fcdc41ffcb03143b7"

$root = Split-Path $PSScriptRoot -Parent
$dest = Join-Path $root "vendor\7-zip"
$marker = Join-Path $dest "version.txt"

if ((Test-Path $marker) -and ((Get-Content $marker -Raw).Trim() -eq $tag)) {
    Write-Output "7-Zip $tag is already in place."
    return
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("maxtrix-7zip-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force $tmp | Out-Null
try {
    foreach ($item in @(@($bootstrap, $bootstrapSha), @($installer, $installerSha))) {
        $file = Join-Path $tmp $item[0]
        Invoke-WebRequest -Uri "https://github.com/ip7z/7zip/releases/download/$tag/$($item[0])" -OutFile $file
        $actual = (Get-FileHash -Algorithm SHA256 $file).Hash.ToLowerInvariant()
        if ($actual -ne $item[1]) {
            throw "Checksum mismatch for $($item[0]): expected $($item[1]), got $actual"
        }
    }
    $out = Join-Path $tmp "out"
    & (Join-Path $tmp $bootstrap) x (Join-Path $tmp $installer) "-o$out" -y 7z.exe 7z.dll License.txt | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "Could not unpack $installer" }

    New-Item -ItemType Directory -Force $dest | Out-Null
    foreach ($name in "7z.exe", "7z.dll", "License.txt") {
        Copy-Item (Join-Path $out $name) (Join-Path $dest $name) -Force
    }
    Set-Content $marker $tag
    Write-Output "7-Zip $tag installed to $dest"
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
