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

# `irm … | iex` executes this text in the caller's own scope, so every function
# and variable declared at the top level would stay behind in the user's
# session. That is not merely untidy. PowerShell retries an unresolved bare
# command with a `Get-` prefix, and the PATH entry written below is invisible
# to the already-running shell — so a leftover `Get-Iomark` used to answer the
# very next `iomark`, silently re-downloading the archive instead of running
# the binary just installed. Everything therefore runs inside a script block,
# which brings its own scope and leaves nothing behind.
& {
    param([string[]]$Arguments)

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

    # ------------------------------------------------------------ arguments

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
        $baseUrl = if ($repo -ne 'feeeei/iomark' -and $version -eq 'latest') {
            "https://github.com/$repo/releases/latest/download"
        }
        elseif ($repo -ne 'feeeei/iomark') {
            "https://github.com/$repo/releases/download/$version"
        }
        elseif ($version -eq 'latest') {
            'https://iomark.dev/releases/latest/download'
        }
        else {
            "https://iomark.dev/releases/download/$version"
        }
    }
    $baseUrl = $baseUrl.TrimEnd('/')

    # ------------------------------------------------------------- download

    function Get-Asset {
        switch ($env:PROCESSOR_ARCHITECTURE) {
            'AMD64' { 'iomark-x86_64-pc-windows-gnu.zip' }
            'ARM64' { 'iomark-aarch64-pc-windows-gnullvm.zip' }
            default { throw "unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
        }
    }

    function Get-IomarkBinary {
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

    # ------------------------------------------------------------- commands

    function Invoke-Install {
        $exe = Get-IomarkBinary
        $tmp = Split-Path $exe
        try {
            New-Item -ItemType Directory -Path $dir -Force | Out-Null
            $target = Join-Path $dir 'iomark.exe'
            Move-Item -Force -LiteralPath $exe -Destination $target
        }
        finally {
            Remove-Item -Recurse -Force -LiteralPath $tmp -ErrorAction SilentlyContinue
        }

        # Never report success on a binary that cannot start. A missing DLL
        # kills the process before main, with no output and no stderr, so
        # without this check a broken build installs quietly and only fails
        # once the user runs it.
        $installed = (& $target --version 2>$null | Select-Object -First 1)
        if ($LASTEXITCODE -ne 0 -or -not $installed) {
            throw "installed $target but it does not run (exit code $LASTEXITCODE)"
        }
        Write-Host "installed $installed -> $target"

        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        if (($userPath -split ';') -notcontains $dir) {
            [Environment]::SetEnvironmentVariable('Path', "$userPath;$dir".Trim(';'), 'User')
            Write-Host "added $dir to your user PATH"
        }
        # The line above only writes the registry; a shell keeps the PATH it
        # was started with, so extend this session too or `iomark` would not
        # resolve until the next terminal.
        if (($env:Path -split ';') -notcontains $dir) {
            $env:Path = "$env:Path;$dir".Trim(';')
        }
        Write-Host 'run: iomark'
    }

    function Invoke-Quick {
        $exe = Get-IomarkBinary
        $tmp = Split-Path $exe
        Write-Host ''
        try {
            & $exe @iomarkArgs
            $code = $LASTEXITCODE
        }
        finally {
            Remove-Item -Recurse -Force -LiteralPath $tmp -ErrorAction SilentlyContinue
        }
        Write-Host ''
        if ($code -ne 0) {
            Write-Warning "iomark exited with code $code"
        }
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
} $Arguments
