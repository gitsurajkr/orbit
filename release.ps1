# Publish a new ORBIT version:   .\release.ps1 0.1.2
#
# Bumps the version in Cargo.toml, checks it builds, commits, tags v0.1.2 and pushes.
# GitHub Actions then builds the installer and publishes the release; every installed ORBIT
# shows an update banner within 30 minutes (or at its next start).

param([Parameter(Mandatory = $true)][string]$Version)
# Not 'Stop': cargo and git print normal progress on stderr, which Windows PowerShell would treat
# as an error. Each native step is checked by its exit code instead.
$ErrorActionPreference = 'Continue'
function Assert-Ok($what) { if ($LASTEXITCODE -ne 0) { throw "$what failed (exit $LASTEXITCODE)" } }

if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "Use a version like 0.1.2" }
Set-Location $PSScriptRoot
if (git status --porcelain) { throw "You have uncommitted changes. Commit or stash them first." }
if (git tag --list "v$Version") { throw "Tag v$Version already exists." }

$cargo = Join-Path $PSScriptRoot 'Cargo.toml'
$text = [IO.File]::ReadAllText($cargo)
$re = [regex]'(?m)^version = "[^"]+"'
$current = $re.Match($text).Value
[IO.File]::WriteAllText($cargo, $re.Replace($text, "version = `"$Version`"", 1))
Write-Host "Version: $current -> version = `"$Version`""

cargo build --release -p app
if ($LASTEXITCODE -ne 0) { git checkout -- Cargo.toml; throw "Build failed; version change undone." }

git add Cargo.toml Cargo.lock; Assert-Ok "git add"
git commit -q -m "release v$Version"; Assert-Ok "git commit"
git tag "v$Version"; Assert-Ok "git tag"
git push origin HEAD "v$Version"; Assert-Ok "git push"
Write-Host ""
Write-Host "Pushed v$Version. GitHub is building the release:"
Write-Host "  https://github.com/gitsurajkr/orbit/actions"
