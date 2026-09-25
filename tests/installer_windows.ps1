# Isolated PowerShell installer tests; no downloads or PATH/profile writes.
param([string]$TemplatePath)
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$source = (Get-Content -LiteralPath $TemplatePath -Raw).Split('# Installer entry point.')[0]
$source = $source.Replace('@@VERSION@@', '3.0.1')
. ([scriptblock]::Create($source))

# Files contain their reported version, so the same tests run on Unix and Windows.
function Get-BinaryVersion([string]$Executable) { return Get-Content -LiteralPath $Executable -Raw }
function Assert-True($Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Assert-Fails([scriptblock]$Action) {
    $failed = $false
    try { & $Action } catch { $failed = $true }
    Assert-True $failed "expected failure"
}

$temporary = Join-Path ([IO.Path]::GetTempPath()) "ngit-installer-test-$([Guid]::NewGuid())"
try {
    # Exercise the real process runner before the file-backed installation tests.
    New-Item -ItemType Directory -Path $temporary -Force | Out-Null
    $pwsh = (Get-Process -Id $PID).Path
    $pidFile = Join-Path $temporary 'probe.pid'
    $escapedPidFile = $pidFile.Replace("'", "''")
    $hang = "[IO.File]::WriteAllText('$escapedPidFile', [string]`$PID); (New-Object Threading.ManualResetEvent(`$false)).WaitOne()"
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($hang))
    $timer = [Diagnostics.Stopwatch]::StartNew()
    Assert-Fails { Invoke-VersionProbe $pwsh "-NoProfile -EncodedCommand $encoded" }
    Assert-True ($timer.Elapsed.TotalSeconds -lt 10) 'version probe exceeded its deadline'
    Assert-True (Test-Path -LiteralPath $pidFile) 'probe did not start'
    $childId = [int](Get-Content -LiteralPath $pidFile)
    Assert-True (-not (Get-Process -Id $childId -ErrorAction SilentlyContinue)) 'timed-out probe still running'
    Assert-Fails { Invoke-VersionProbe $pwsh '-NoProfile -Command "exit 17"' }
    Assert-True ((Invoke-VersionProbe $pwsh '--version') -like 'PowerShell *') 'successful probe lost output'

    $destination = Join-Path $temporary 'destination with spaces'
    $release = Join-Path $temporary 'release'
    New-Item -ItemType Directory -Path $destination, $release -Force | Out-Null
    $sources = @{}
    foreach ($binary in $binaryNames) {
        $sources[$binary] = Join-Path $release $binary
        $reported = if ($binary -eq 'ngit.exe') { 'ngit 3.0.1' } else { 'v3.0.1' }
        [IO.File]::WriteAllText($sources[$binary], $reported)
    }

    # Cargo is selected automatically and keeps its custom root; no real Cargo runs.
    $cargoRootFixture = Join-Path $temporary 'custom Cargo root'
    $cargoBin = Join-Path $cargoRootFixture 'bin'
    New-Item -ItemType Directory -Path $cargoBin -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $cargoRootFixture '.crates2.json'), '{}')
    [IO.File]::WriteAllText((Join-Path $cargoBin 'ngit.exe'), 'ngit 1.6.0')
    function Get-Command {
        param($Name, $CommandType, $ErrorAction)
        if ($Name -eq 'ngit') { return [pscustomobject]@{ Source = (Join-Path $cargoBin 'ngit.exe') } }
        return $null
    }
    function cargo { $script:cargoArgs = @($args); $script:LASTEXITCODE = 0 }
    Invoke-NgitInstall
    Assert-True (($cargoArgs -join '|') -eq "install|ngit|--locked|--version|3.0.1|--root|$cargoRootFixture") 'automatic Cargo root or version was lost'
    # Unverifiable lookup must not block an otherwise owned Cargo update.
    $savedPath = $env:PATH
    try {
        $env:PATH += [IO.Path]::PathSeparator
        Invoke-NgitInstall
        Assert-True (($cargoArgs -join '|') -eq "install|ngit|--locked|--version|3.0.1|--root|$cargoRootFixture") 'unverifiable PATH blocked Cargo'
    } finally { $env:PATH = $savedPath }
    $Method = 'cargo'
    Invoke-NgitInstall
    Assert-True (($cargoArgs -join '|') -eq "install|ngit|--locked|--version|3.0.1|--root|$cargoRootFixture") 'Cargo root or version was lost'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $cargoBin $receiptFilename))) 'Cargo installation was adopted'
    function cargo { $script:LASTEXITCODE = 17 }
    $Method = 'auto'
    Assert-Fails { Invoke-NgitInstall }
    Assert-True ((Get-BinaryVersion (Join-Path $cargoBin 'ngit.exe')) -eq 'ngit 1.6.0') 'failed Cargo update changed the installation'
    Remove-Item Function:Get-Command, Function:cargo
    $Method = 'auto'

    # Git's exec-path wins over an otherwise complete Cargo installation.
    $execDirectory = Join-Path $temporary 'git exec'
    New-Item -ItemType Directory -Path $execDirectory | Out-Null
    $oldHelper = Join-Path $execDirectory 'git-remote-nostr.exe'
    [IO.File]::WriteAllText($oldHelper, 'v2.6.1')
    $originalExecProbe = ${function:Get-GitExecPath}
    function Get-GitExecPath { param($Executable); return $execDirectory }
    function Get-Command {
        param($Name, $CommandType, $ErrorAction)
        if ($Name -eq 'git') { return [pscustomobject]@{ Source = 'fake-git' } }
        if ($Name -eq 'ngit') { return [pscustomobject]@{ Source = (Join-Path $cargoBin 'ngit.exe') } }
        return $null
    }
    function cargo { throw 'conflicting installation must not invoke Cargo' }
    Assert-True ((Get-GitHelper) -eq (Get-CanonicalPath $oldHelper)) 'Git exec-path helper was missed'
    Assert-Fails { Invoke-NgitInstall }
    Assert-True ((Get-BinaryVersion $oldHelper) -eq 'v2.6.1') 'conflicting helper was modified'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $cargoBin $receiptFilename))) 'conflict adopted Cargo'
    Remove-Item Function:Get-Command, Function:cargo
    Set-Item Function:Get-GitExecPath $originalExecProbe

    # Fresh install and missing-helper repair.
    Install-Binaries $destination $sources
    Assert-True (Test-Receipt $destination) 'receipt was not installed'
    Remove-Item -LiteralPath (Join-Path $destination 'git-remote-nostr.exe')
    Install-Binaries $destination $sources
    Assert-True (Test-Path -LiteralPath (Join-Path $destination 'git-remote-nostr.exe')) 'helper was not repaired'

    # Unreceipted files and downgrades remain untouched.
    Remove-Item -LiteralPath (Join-Path $destination $receiptFilename) -Force
    Assert-Fails { Install-Binaries $destination $sources }
    Write-Receipt $destination
    [IO.File]::WriteAllText((Join-Path $destination 'ngit.exe'), 'ngit 4.0.0-rc.1')
    Assert-Fails { Install-Binaries $destination $sources }
    Assert-True ((Get-BinaryVersion (Join-Path $destination 'ngit.exe')) -eq 'ngit 4.0.0-rc.1') 'downgrade changed existing binary'
    $AllowDowngrade = $true
    Install-Binaries $destination $sources
    $AllowDowngrade = $false

    # Corrupt receipts require explicit repair.
    [IO.File]::WriteAllText((Join-Path $destination $receiptFilename), 'broken')
    Assert-Fails { Install-Binaries $destination $sources }
    $Repair = $true
    Install-Binaries $destination $sources
    $Repair = $false
    Assert-True (Test-Receipt $destination) 'receipt was not repaired'

    # Fail the second move and verify all original bytes are restored.
    [IO.File]::WriteAllText((Join-Path $destination 'ngit.exe'), 'ngit 1.6.0')
    [IO.File]::WriteAllText((Join-Path $destination 'git-remote-nostr.exe'), 'old helper')
    function Move-Item {
        param([string]$LiteralPath, [string]$Destination, [switch]$Force)
        if ($LiteralPath -match 'new[/\\]git-remote-nostr.exe$') { throw 'injected replacement failure' }
        Microsoft.PowerShell.Management\Move-Item -LiteralPath $LiteralPath -Destination $Destination -Force:$Force
    }
    Assert-Fails { Install-Binaries $destination $sources }
    Remove-Item Function:Move-Item
    Assert-True ((Get-BinaryVersion (Join-Path $destination 'ngit.exe')) -eq 'ngit 1.6.0') 'ngit was not restored'
    Assert-True ((Get-BinaryVersion (Join-Path $destination 'git-remote-nostr.exe')) -eq 'old helper') 'helper was not restored'
    Assert-True (Test-Receipt $destination) 'receipt was not preserved'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $destination '.ngit-install-lock'))) 'lock was not released'

    # A second process cannot replace files while the lock exists.
    New-Item -ItemType Directory -Path (Join-Path $destination '.ngit-install-lock') | Out-Null
    Assert-Fails { Install-Binaries $destination $sources }
    Assert-True (Test-Path -LiteralPath (Join-Path $destination '.ngit-install-lock')) 'another installer removed the lock'
    Write-Host 'PowerShell installer behaviour tests passed.'
} finally {
    Remove-Item -LiteralPath $temporary -Recurse -Force -ErrorAction SilentlyContinue
}
