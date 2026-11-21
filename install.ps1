# Installs Kito on Windows from a GitHub release: kito, kitod, kito-mcp and
# kito-server, in %LOCALAPPDATA%\Programs\kito unless KITO_INSTALL_DIR says
# otherwise.
#
#   irm https://github.com/KitoOrg/kito/releases/latest/download/install.ps1 | iex
#
# $env:KITO_VERSION = '0.19.0'   a version instead of the latest
# $env:KITO_INSTALL_DIR = 'DIR'  where the programs go
# $env:GITHUB_TOKEN = '...'      to read the releases of a private repository
#
# Checks that run when a lane is integrated are written for `sh`: Kito uses
# the one of Git for Windows when it is on the PATH, and `cmd` otherwise.

$ErrorActionPreference = 'Stop'
# Windows PowerShell 5.1 draws a progress bar that makes downloads slow.
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$repo = 'KitoOrg/kito'
$dir = if ($env:KITO_INSTALL_DIR) { $env:KITO_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\kito' }

function Say($text) { Write-Host "kito: $text" }

if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64' -and $env:PROCESSOR_ARCHITEW6432 -ne 'AMD64') {
    throw "kito: no build for $($env:PROCESSOR_ARCHITECTURE): Kito for Windows is x86_64"
}

function Headers($accept) {
    $headers = @{ Accept = $accept; 'User-Agent' = 'kito-install' }
    if ($env:GITHUB_TOKEN) { $headers.Authorization = "Bearer $($env:GITHUB_TOKEN)" }
    $headers
}

$api = "https://api.github.com/repos/$repo/releases"
try {
    if ($env:KITO_VERSION) {
        $wanted = $env:KITO_VERSION.TrimStart('v')
        $release = Invoke-RestMethod -Uri "$api/tags/v$wanted" -Headers (Headers 'application/vnd.github+json')
    } else {
        $release = Invoke-RestMethod -Uri "$api/latest" -Headers (Headers 'application/vnd.github+json')
    }
} catch {
    throw "kito: no release found (a private repository needs GITHUB_TOKEN): $($_.Exception.Message)"
}
$version = $release.tag_name.TrimStart('v')
$name = "kito-$version-windows-x86_64"

# The API URL of an asset: works for private repositories too, with the token.
function Fetch($asset, $to) {
    $found = $release.assets | Where-Object { $_.name -eq $asset } | Select-Object -First 1
    if (-not $found) { throw "kito: the release $version has no $asset" }
    Invoke-WebRequest -Uri $found.url -Headers (Headers 'application/octet-stream') -OutFile $to -UseBasicParsing
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) "kito-install-$([Guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Say "downloading $name"
    $zip = Join-Path $tmp "$name.zip"
    Fetch "$name.zip" $zip
    Fetch "$name.zip.sha256" "$zip.sha256"

    $expected = ((Get-Content "$zip.sha256" -TotalCount 1) -split '\s+')[0].ToLower()
    $actual = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLower()
    if ($expected -ne $actual) { throw "kito: the checksum of $name.zip does not match: not installed" }

    Expand-Archive -Path $zip -DestinationPath $tmp
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    foreach ($bin in 'kito', 'kitod', 'kito-mcp', 'kito-server') {
        try {
            Copy-Item -Force -Path (Join-Path $tmp "$name\$bin.exe") -Destination (Join-Path $dir "$bin.exe")
        } catch {
            throw "kito: could not write $bin.exe in $dir (a daemon that is running holds it: ``kito daemon stop`` in its repository, then again): $($_.Exception.Message)"
        }
    }
} finally {
    Remove-Item -Recurse -Force -Path $tmp -ErrorAction SilentlyContinue
}

Say "Kito $version is in $dir"
if (-not (($env:Path -split ';') -contains $dir)) {
    Say "add it to your PATH, for you alone:"
    Say "  [Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path', 'User') + ';$dir', 'User')"
}
