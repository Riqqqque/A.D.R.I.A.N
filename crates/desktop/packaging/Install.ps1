# Installs the A.D.R.I.A.N. PC app for this Windows user: copies it to
# %LOCALAPPDATA%\A.D.R.I.A.N., adds it to the desktop and the Start menu,
# starts it with Windows (in the tray), pairs it, and starts it now.
#
#   powershell -ExecutionPolicy Bypass -File crates\desktop\packaging\Install.ps1 -Server myserver:8791 -Token sat_xxxx
#
# -App      the built app (default: target\release\adrian-desktop.exe in this repo)
# -Server   the bot's address and satellite port, e.g. myserver:8791
# -Token    the sat_ token from `sudo adrian satellite add NAME OWNER_ID`
# -NoStartup  don't start it with Windows
# -Uninstall  remove the app and its shortcuts (settings are kept)
param(
    [string]$App = (Join-Path $PSScriptRoot '..\..\..\target\release\adrian-desktop.exe'),
    [string]$Server,
    [string]$Token,
    [switch]$NoStartup,
    [switch]$Uninstall
)
$ErrorActionPreference = 'Stop'
$dir = Join-Path $env:LOCALAPPDATA 'A.D.R.I.A.N'
$exe = Join-Path $dir 'ADRIAN.exe'
$links = @(
    (Join-Path ([Environment]::GetFolderPath('Desktop')) 'A.D.R.I.A.N..lnk'),
    (Join-Path ([Environment]::GetFolderPath('Programs')) 'A.D.R.I.A.N..lnk'),
    (Join-Path ([Environment]::GetFolderPath('Startup')) 'A.D.R.I.A.N..lnk')
)

# A running copy holds the file open.
Get-Process ADRIAN -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } | Stop-Process -Force
Start-Sleep -Milliseconds 500

if ($Uninstall) {
    $links | Where-Object { Test-Path $_ } | Remove-Item -Force
    if (Test-Path $dir) { Remove-Item -Recurse -Force $dir }
    Write-Host 'A.D.R.I.A.N. is removed. Its settings stay in %APPDATA%\adrian.'
    exit 0
}

if (-not (Test-Path $App)) { throw "Build the app first (cargo build --release -p adrian-desktop), or pass -App <path to adrian-desktop.exe>." }
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item $App $exe -Force

if ($Server -and $Token) {
    & $exe --pair $Server $Token
    if ($LASTEXITCODE -ne 0) { throw 'Pairing failed: check the server address and the sat_ token.' }
} elseif ($Server -or $Token) {
    throw 'Give both -Server and -Token, or neither (then enter them in the app''s settings).'
}

$shell = New-Object -ComObject WScript.Shell
function Add-Shortcut([string]$path, [string]$arguments) {
    $l = $shell.CreateShortcut($path)
    $l.TargetPath = $exe
    $l.Arguments = $arguments
    $l.WorkingDirectory = $dir
    $l.IconLocation = "$exe,0"
    $l.Description = 'A.D.R.I.A.N.'
    $l.Save()
}
Add-Shortcut $links[0] ''
Add-Shortcut $links[1] ''
if ($NoStartup) {
    if (Test-Path $links[2]) { Remove-Item -Force $links[2] }
} else {
    Add-Shortcut $links[2] '--background'
}

# Not paired yet: open the window so the settings are right there.
if ($Server -and $Token) { Start-Process -FilePath $exe -ArgumentList '--background' } else { Start-Process -FilePath $exe }
Write-Host 'A.D.R.I.A.N. is installed: on the desktop, in the Start menu, and running in the tray.'
