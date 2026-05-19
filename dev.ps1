# Developer wrapper for common workspace tasks. Run `./dev.ps1 help` to see
# the list of subcommands. See DEV.md for the workflow details.
#
# Windows-targeted port of `dev.sh`. Same command surface, same arguments.

$ErrorActionPreference = 'Stop'

# Capture the command + the remaining args without declaring a `param()`
# block. A `param()` block would force PowerShell to interpret dash-prefixed
# tokens (e.g. `./dev.ps1 pydl -l debug …`) as parameter names of *this*
# script and fail before the values reach cargo. Reading `$args` directly
# keeps every token as a positional string.
if ($args.Count -eq 0) {
    $cmd = 'help'
    $rest = @()
} else {
    $cmd = $args[0]
    $rest = if ($args.Count -gt 1) { @($args[1..($args.Count - 1)]) } else { @() }
}

function Show-Usage {
    @'
Usage: ./dev.ps1 <command> [args...]

Commands:
  build                      cargo build --workspace --all-targets
  test [args...]             cargo test --workspace --all-targets [args...]
  lint                       cargo clippy --workspace --all-targets -- -D warnings
  fmt                        cargo +nightly fmt --all
  fmt-check                  cargo +nightly fmt --all -- --check
  check                      fmt-check + lint + test (what CI should run)
  pydl [args...]             cargo run -p pydl --quiet -- [args...]
  get-checksums [args...]    cargo run -p get-checksums -- [args...]
  check-checksums [args...]  cargo run -p check-checksums -- [args...]
  install-pydl               build pydl in release mode and copy it to $HOME\.local\bin
  clean                      cargo clean
  help                       show this help

Environment:
  CARGO     override the cargo binary (default: cargo). Does not affect fmt /
            fmt-check / check, which invoke `cargo +nightly fmt` for grouped
            import ordering (see rustfmt.toml).
'@ | Write-Host
}

# Wrap external invocations: `$ErrorActionPreference = 'Stop'` only fires on
# PowerShell-side errors, not on a non-zero native exit code, so we have to
# check `$LASTEXITCODE` ourselves and propagate the failure.
function Invoke-Native {
    param(
        [Parameter(Mandatory = $true)][string]$Exe,
        [string[]]$ArgList = @()
    )
    & $Exe @ArgList
    if ($LASTEXITCODE -ne 0) {
        throw "$Exe exited with code $LASTEXITCODE"
    }
}

$cargo = if ($env:CARGO) { $env:CARGO } else { 'cargo' }

# PowerShell's `switch` runs every matching clause unless `break` is used —
# add `break` after each so a future literal/script-block overlap can't
# silently double-fire.
switch ($cmd) {
    'build' {
        Invoke-Native $cargo (@('build', '--workspace', '--all-targets') + $rest)
        break
    }
    'test' {
        Invoke-Native $cargo (@('test', '--workspace', '--all-targets') + $rest)
        break
    }
    'lint' {
        Invoke-Native $cargo @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings')
        break
    }
    'fmt' {
        Invoke-Native 'cargo' @('+nightly', 'fmt', '--all')
        break
    }
    'fmt-check' {
        Invoke-Native 'cargo' @('+nightly', 'fmt', '--all', '--', '--check')
        break
    }
    'check' {
        Invoke-Native 'cargo' @('+nightly', 'fmt', '--all', '--', '--check')
        Invoke-Native $cargo @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings')
        Invoke-Native $cargo @('test', '--workspace', '--all-targets')
        break
    }
    'pydl' {
        Invoke-Native $cargo (@('run', '-p', 'pydl', '--quiet', '--') + $rest)
        break
    }
    'get-checksums' {
        Invoke-Native $cargo (@('run', '--bin', 'get-checksums', '--') + $rest)
        break
    }
    'check-checksums' {
        Invoke-Native $cargo (@('run', '--bin', 'check-checksums', '--') + $rest)
        break
    }
    'install-pydl' {
        Invoke-Native $cargo @('build', '-p', 'pydl', '--release')
        # Build paths from one segment per Join-Path call so the OS-native
        # separator wins. Targets Windows but stays correct under macOS /
        # Linux pwsh in case someone runs from WSL or a cross-platform CI.
        $destDir = Join-Path (Join-Path $HOME '.local') 'bin'
        if (-not (Test-Path -LiteralPath $destDir)) {
            New-Item -ItemType Directory -Path $destDir -Force | Out-Null
        }
        $src = Join-Path (Join-Path 'target' 'release') 'pydl.exe'
        $dest = Join-Path $destDir 'pydl.exe'
        Copy-Item -LiteralPath $src -Destination $dest -Force
        Write-Host "installed pydl -> $dest"
        # Windows PATH separator is `;`. Compare case-insensitively because
        # NTFS path comparisons usually are.
        $onPath = ($env:PATH -split ';') |
            Where-Object { $_ -and ($_.TrimEnd('\') -ieq $destDir.TrimEnd('\')) }
        if (-not $onPath) {
            [Console]::Error.WriteLine("note: $destDir is not on your PATH")
        }
        break
    }
    'clean' {
        Invoke-Native $cargo @('clean')
        break
    }
    { $_ -in @('help', '-h', '--help') } {
        Show-Usage
        break
    }
    default {
        [Console]::Error.WriteLine("error: unknown command '$cmd'")
        Show-Usage
        exit 2
    }
}
