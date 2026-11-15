# Installs spotdl and its desktop app for the current user on Windows, with a Start menu
# entry and its icon.
#
#   powershell -ExecutionPolicy Bypass -File install.ps1              build, then install
#   powershell -ExecutionPolicy Bypass -File install.ps1 -Uninstall   remove it again
#
# The programs go to %LOCALAPPDATA%\Programs\spotdl, which is added to your PATH. The
# login and settings (in %APPDATA%\spotdl) are kept either way.
param([switch]$Uninstall)
$ErrorActionPreference = 'Stop'

$dir = Join-Path $env:LOCALAPPDATA 'Programs\spotdl'
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'spotdl.lnk'
$envKey = 'HKCU:\Environment'

# The user's PATH as it's stored, with any %VARIABLES% in it left as they are.
function Get-UserPath {
    $value = (Get-Item $envKey).GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    @($value -split ';' | Where-Object { $_ -ne '' })
}

function Set-UserPath([string[]]$entries) {
    Set-ItemProperty -Path $envKey -Name Path -Value ($entries -join ';') -Type ExpandString
    # setting any variable this way tells Explorer (and the terminals it starts from now
    # on) to read the environment again
    [Environment]::SetEnvironmentVariable('SPOTDL_INSTALL', '1', 'User')
    [Environment]::SetEnvironmentVariable('SPOTDL_INSTALL', $null, 'User')
}

if ($Uninstall) {
    try {
        Remove-Item -Force -ErrorAction SilentlyContinue $shortcut
        if (Test-Path $dir) { Remove-Item -Recurse -Force $dir }
    } catch {
        throw "Couldn't remove $dir; if spotdl is running, quit it and try again. ($_)"
    }
    $path = Get-UserPath
    if ($path -contains $dir) { Set-UserPath @($path | Where-Object { $_ -ne $dir }) }
    Write-Host 'Removed spotdl.'
    exit 0
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'cargo was not found. Install Rust from https://rustup.rs first (see the README).'
}
Set-Location $PSScriptRoot
cargo build --release --locked
if ($LASTEXITCODE -ne 0) { throw 'Building spotdl failed (see above).' }

New-Item -ItemType Directory -Force -Path $dir | Out-Null
try {
    $files = 'target\release\spotdl.exe', 'target\release\spotdl-gui.exe', 'assets\icon\spotdl.ico'
    Copy-Item -Force -Path $files -Destination $dir
} catch {
    throw "Couldn't copy the programs to $dir; if spotdl is running, quit it and try again. ($_)"
}

$shell = New-Object -ComObject WScript.Shell
$link = $shell.CreateShortcut($shortcut)
$link.TargetPath = Join-Path $dir 'spotdl-gui.exe'
$link.WorkingDirectory = $dir
$link.IconLocation = (Join-Path $dir 'spotdl.ico') + ',0'
$link.Description = 'Search Spotify and download songs, albums and playlists'
$link.Save()

$path = Get-UserPath
if ($path -notcontains $dir) {
    Set-UserPath ($path + $dir)
    Write-Host "Added $dir to your PATH; open a new terminal to use spotdl there."
}
Write-Host "Installed spotdl: find it in the Start menu, or run spotdl-gui. The programs are in $dir."
