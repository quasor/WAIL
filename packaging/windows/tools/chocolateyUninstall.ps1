$ErrorActionPreference = 'Stop'

$installDir = "$env:ProgramFiles\WAIL"

# --- Remove plugins using registry paths ---
$pluginKey = "HKLM:\Software\WAIL\PluginPaths"
if (Test-Path $pluginKey) {
    $props = Get-ItemProperty -Path $pluginKey -ErrorAction SilentlyContinue

    if ($props.VST3Dir) {
        Remove-Item "$($props.VST3Dir)\wail-plugin-send.vst3" -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item "$($props.VST3Dir)\wail-plugin-recv.vst3" -Recurse -Force -ErrorAction SilentlyContinue
    }
    if ($props.CLAPDir) {
        Remove-Item "$($props.CLAPDir)\wail-plugin-send.clap" -Force -ErrorAction SilentlyContinue
        Remove-Item "$($props.CLAPDir)\wail-plugin-recv.clap" -Force -ErrorAction SilentlyContinue
        $remaining = Get-ChildItem "$($props.CLAPDir)\wail-*" -ErrorAction SilentlyContinue
        if (-not $remaining) {
            Remove-Item "$($props.CLAPDir)\opus.dll" -Force -ErrorAction SilentlyContinue
        }
    }

    Remove-Item "HKLM:\Software\WAIL" -Recurse -Force -ErrorAction SilentlyContinue
}

# --- Remove shortcuts ---
Remove-Item "$env:PUBLIC\Desktop\WAIL.lnk" -Force -ErrorAction SilentlyContinue
$startMenu = "$env:ProgramData\Microsoft\Windows\Start Menu\Programs"
Remove-Item "$startMenu\WAIL.lnk" -Force -ErrorAction SilentlyContinue

# --- Remove application directory ---
if (Test-Path $installDir) {
    Remove-Item $installDir -Recurse -Force -ErrorAction SilentlyContinue
}

# --- Remove Add/Remove Programs registry entry ---
Remove-Item "HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\WAIL" -Force -ErrorAction SilentlyContinue

Write-Host "WAIL has been uninstalled."
