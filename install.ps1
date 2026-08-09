# iomark installer for Windows PowerShell — https://iomark.dev
#
#   irm https://iomark.dev/install.ps1 | iex
#
# To pass a command or arguments, run the script as a scriptblock:
#
#   & ([scriptblock]::Create((irm https://iomark.dev/install.ps1))) quick
#   & ([scriptblock]::Create((irm https://iomark.dev/install.ps1))) quick D: --size 4GiB
#   & ([scriptblock]::Create((irm https://iomark.dev/install.ps1))) uninstall
#
# Arguments are parsed by hand rather than by a typed param() block: PowerShell
# would otherwise bind `quick D:` positionally, taking the target disk as the
# release version.

param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Arguments
)

$ErrorActionPreference = 'Stop'

# Windows PowerShell 5.1 on older builds still negotiates TLS 1.0, which
# github.com refuses.
try {
    [Net.ServicePointManager]::SecurityProtocol =
    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
}
catch {}

$repo = if ($env:IOMARK_REPO) { $env:IOMARK_REPO } else { 'feeeei/iomark' }
$version = if ($env:IOMARK_VERSION) { $env:IOMARK_VERSION } else { 'latest' }
$dir = $env:IOMARK_INSTALL_DIR
$baseUrl = $env:IOMARK_BASE_URL
$command = 'install'
$iomarkArgs = @()

function Show-Usage {
    Write-Host @'
iomark installer

Commands:
  install      Download and install iomark (default)
  quick        Download to a temp folder, run the benchmark, delete it
  uninstall    Remove a previously installed iomark
  help         Show this help

Options:
  -Version <tag>   Release to fetch, e.g. v0.1.0 (default: latest)
  -Dir <path>      Install directory (default: %LOCALAPPDATA%\Programs\iomark)

Environment:
  IOMARK_VERSION, IOMARK_INSTALL_DIR, IOMARK_REPO, IOMARK_BASE_URL

Anything else after the command is forwarded to iomark by `quick`, so
`quick D: --size 4GiB` benchmarks D: with a 4 GiB test file.
'@
}

# ---------------------------------------------------------------- arguments

# @($null) has one element, so an unbound -Arguments must be filtered out
# rather than counted.
$rest = @()
if ($Arguments) { $rest = @($Arguments | Where-Object { $_ }) }

if ($rest.Count -gt 0 -and $rest[0] -in @('install', 'quick', 'uninstall', 'help')) {
    $command = $rest[0]
    $rest = @($rest | Select-Object -Skip 1)
}

function Get-OptionValue([array]$argv, [int]$index, [string]$name) {
    if ($index -ge $argv.Count) { throw "$name needs a value" }
    [string]$argv[$index]
}

$i = 0
while ($i -lt $rest.Count) {
    $arg = [string]$rest[$i]
    if ($arg -match '^-{1,2}(v|version)$') {
        $version = Get-OptionValue $rest ($i + 1) $arg; $i += 2; continue
    }
    if ($arg -match '^-{1,2}(d|dir)$') {
        $dir = Get-OptionValue $rest ($i + 1) $arg; $i += 2; continue
    }
    if ($arg -match '^-{1,2}(h|help|\?)$') { Show-Usage; return }
    # First non-option argument: everything from here belongs to iomark.
    if ($arg -eq '--') { $i++ }
    $iomarkArgs = @($rest | Select-Object -Skip $i)
    break
}

if (-not $dir) { $dir = Join-Path $env:LOCALAPPDATA 'Programs\iomark' }
if (-not $baseUrl) {
    $baseUrl = if ($version -eq 'latest') {
        "https://github.com/$repo/releases/latest/download"
    }
    else {
        "https://github.com/$repo/releases/download/$version"
    }
}
$baseUrl = $baseUrl.TrimEnd('/')

# ----------------------------------------------------------------- download

function Get-Asset {
    switch ($env:PROCESSOR_ARCHITECTURE) {
        'AMD64' { 'iomark-x86_64-pc-windows-gnu.zip' }
        'ARM64' { 'iomark-aarch64-pc-windows-gnullvm.zip' }
        default { throw "unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
    }
}

function Get-Iomark {
    # Downloads and unpacks into a fresh temp folder; returns the exe path.
    $asset = Get-Asset
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('iomark-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp -Force | Out-Null
    $zip = Join-Path $tmp $asset
    $sums = Join-Path $tmp 'SHA256SUMS'

    Write-Host "downloading $asset ($version)"
    # Progress rendering makes Invoke-WebRequest an order of magnitude slower.
    $saved = $ProgressPreference
    $ProgressPreference = 'SilentlyContinue'
    try {
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/$asset" -OutFile $zip
        }
        catch {
            throw "download failed: $baseUrl/$asset"
        }
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/SHA256SUMS" -OutFile $sums
        }
        catch {
            $sums = $null
            Write-Warning 'could not download SHA256SUMS, skipping checksum verification'
        }
    }
    finally {
        $ProgressPreference = $saved
    }

    if ($sums) {
        $expected = $null
        foreach ($line in Get-Content $sums) {
            $parts = $line -split '\s+', 2
            if ($parts.Count -eq 2 -and $parts[1].Trim().TrimStart('*') -eq $asset) {
                $expected = $parts[0]
            }
        }
        if (-not $expected) { throw "$asset is missing from SHA256SUMS" }
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $zip).Hash
        if ($actual -ne $expected.Trim()) {
            throw "checksum mismatch for $asset (expected $expected, got $actual)"
        }
    }

    Expand-Archive -Force -LiteralPath $zip -DestinationPath $tmp
    $exe = Join-Path $tmp 'iomark.exe'
    if (-not (Test-Path -LiteralPath $exe)) { throw "iomark.exe not found in $asset" }
    $exe
}

# ----------------------------------------------------------------- commands

function Invoke-Install {
    $exe = Get-Iomark
    $tmp = Split-Path $exe
    try {
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
        $target = Join-Path $dir 'iomark.exe'
        Move-Item -Force -LiteralPath $exe -Destination $target
    }
    finally {
        Remove-Item -Recurse -Force -LiteralPath $tmp -ErrorAction SilentlyContinue
    }

    $installed = (& $target --version 2>$null | Select-Object -First 1)
    Write-Host "installed $installed -> $target"

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (($userPath -split ';') -notcontains $dir) {
        [Environment]::SetEnvironmentVariable('Path', "$userPath;$dir".Trim(';'), 'User')
        Write-Host "added $dir to your user PATH — open a new terminal, then run: iomark"
    }
    else {
        Write-Host 'run: iomark'
    }
}

function Invoke-Quick {
    $exe = Get-Iomark
    $tmp = Split-Path $exe
    Write-Host ''
    try {
        & $exe @iomarkArgs
    }
    finally {
        Remove-Item -Recurse -Force -LiteralPath $tmp -ErrorAction SilentlyContinue
    }
    Write-Host ''
    Write-Host 'that binary was temporary — install it with:'
    Write-Host '  irm https://iomark.dev/install.ps1 | iex'
}

function Invoke-Uninstall {
    $removed = $false
    $candidates = @($dir, (Join-Path $env:LOCALAPPDATA 'Programs\iomark')) | Select-Object -Unique
    foreach ($candidate in $candidates) {
        $target = Join-Path $candidate 'iomark.exe'
        if (Test-Path -LiteralPath $target) {
            Remove-Item -Force -LiteralPath $target
            Write-Host "removed $target"
            $removed = $true
        }
    }
    if (-not $removed) { Write-Host 'no iomark installation found' }
}

switch ($command) {
    'install' { Invoke-Install }
    'quick' { Invoke-Quick }
    'uninstall' { Invoke-Uninstall }
    'help' { Show-Usage }
}
