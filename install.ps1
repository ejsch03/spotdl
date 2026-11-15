# Builds spotdl's Windows installer, then installs spotdl with it: for the current user, in
# %LOCALAPPDATA%\Programs\spotdl, with a Start menu entry, on the PATH, and in Settings > Apps
# to uninstall it.
#
#   powershell -ExecutionPolicy Bypass -File install.ps1              build, then install
#   powershell -ExecutionPolicy Bypass -File install.ps1 -BuildOnly   only build the installer
#   powershell -ExecutionPolicy Bypass -File install.ps1 -Uninstall   remove it again
#
# The installer, target\installer\spotdl-<version>-<arch>-setup.exe, installs spotdl on any
# Windows 10 or 11 PC. Add -Target aarch64-pc-windows-msvc to build it for Windows on ARM.
# Building it needs Inno Setup 6.7 or later: winget install JRSoftware.InnoSetup.7
#
# The login and settings (in %APPDATA%\spotdl) are kept either way.
param(
    [switch]$BuildOnly,
    [switch]$Uninstall,
    [string]$Target
)
$ErrorActionPreference = 'Stop'

# where Windows lists spotdl once it's installed (named after the installer's AppId, see
# assets\windows\spotdl.iss)
$appKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{F8C04EF7-E87E-41A3-B99A-AF43699A8A4A}_is1'

if ($Uninstall) {
    # installed for the current user, or else for everyone
    $app = 'HKCU:', 'HKLM:' |
        ForEach-Object { Get-ItemProperty "$_\$appKey" -ErrorAction SilentlyContinue } |
        Select-Object -First 1
    if (-not $app) {
        Write-Host "spotdl isn't installed."
        exit 0
    }
    $uninstaller = Start-Process -FilePath $app.UninstallString.Trim('"') -Wait -PassThru `
        -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART'
    if ($uninstaller.ExitCode -ne 0) {
        throw "Uninstalling spotdl failed (exit code $($uninstaller.ExitCode))."
    }
    # the uninstaller removes the folder itself just after it's done
    for ($i = 0; $i -lt 50 -and (Test-Path $app.InstallLocation); $i++) { Start-Sleep -Milliseconds 200 }
    Write-Host 'Removed spotdl.'
    exit 0
}

# Inno Setup's compiler, wherever Inno Setup 7 or 6 was installed (for everyone or just this
# user), or else on the PATH
function Find-Iscc {
    foreach ($version in 7, 6) {
        foreach ($root in 'HKCU:\Software', 'HKLM:\SOFTWARE', 'HKLM:\SOFTWARE\WOW6432Node') {
            $key = "$root\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup ${version}_is1"
            $location = (Get-ItemProperty $key -ErrorAction SilentlyContinue).InstallLocation
            if ($location -and (Test-Path (Join-Path $location 'ISCC.exe'))) {
                return Join-Path $location 'ISCC.exe'
            }
        }
    }
    (Get-Command ISCC.exe -ErrorAction SilentlyContinue).Source
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'cargo was not found. Install Rust from https://rustup.rs first (see the README).'
}
$iscc = Find-Iscc
if (-not $iscc) {
    throw 'Inno Setup, which makes the installer, was not found. Install it with: winget install JRSoftware.InnoSetup.7'
}

Set-Location $PSScriptRoot
$metadata = cargo metadata --no-deps --format-version 1 --locked | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed (see above).' }
$version = $metadata.packages[0].version
$targetDir = $metadata.target_directory

$build = 'build', '--release', '--locked'
if ($Target) {
    $build += '--target', $Target
    $binDir = Join-Path $targetDir "$Target\release"
} else {
    # the programs are built for this PC
    $Target = (rustc -vV | Where-Object { $_ -like 'host: *' }) -replace '^host: ', ''
    $binDir = Join-Path $targetDir 'release'
}
$arch = switch -Wildcard ($Target) {
    'x86_64-*' { 'x64' }
    'aarch64-*' { 'arm64' }
    default { throw "The installer is for x64 and ARM64 Windows, not $Target." }
}
cargo @build
if ($LASTEXITCODE -ne 0) { throw 'Building spotdl failed (see above).' }

$outDir = Join-Path $targetDir 'installer'
& $iscc /Q "/DArch=$arch" "/DBinDir=$binDir" "/O$outDir" assets\windows\spotdl.iss
if ($LASTEXITCODE -ne 0) { throw 'Making the installer failed (see above).' }
$installer = Join-Path $outDir "spotdl-$version-$arch-setup.exe"
if ($BuildOnly) {
    Write-Host "Made the installer: $installer"
    exit 0
}

# Without asking anything: for the current user, or for everyone if that's how spotdl was
# installed before (which asks for admin rights). A running spotdl is closed first.
$setup = Start-Process -FilePath $installer -Wait -PassThru `
    -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART'
if ($setup.ExitCode -ne 0) {
    throw "Installing spotdl failed (exit code $($setup.ExitCode)); run $installer to see why."
}
$dir = (Get-ItemProperty "HKCU:\$appKey", "HKLM:\$appKey" -ErrorAction SilentlyContinue |
    Select-Object -First 1).InstallLocation
Write-Host "Installed spotdl in ${dir}: find it in the Start menu, or run spotdl-gui."
Write-Host 'Open a new terminal to run spotdl there.'
