$ErrorActionPreference = 'Stop'

$toolsDir = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"

# --- Parse parameters ---
$pp = Get-PackageParameters

$vst3Dir = if ($pp['VST3Dir']) { $pp['VST3Dir'] } else { "$env:COMMONPROGRAMFILES\VST3" }
$clapDir = if ($pp['CLAPDir']) { $pp['CLAPDir'] } else { "$env:COMMONPROGRAMFILES\CLAP" }

# --- Detect and remove existing NSIS installation ---
$nsisUninstKey = "HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\WAIL"
if (Test-Path $nsisUninstKey) {
    $existing = Get-ItemProperty -Path $nsisUninstKey -ErrorAction SilentlyContinue
    if ($existing.UninstallString -and $existing.UninstallString -notlike "*choco*") {
        Write-Host "Removing existing NSIS installation..."
        $uninstaller = $existing.UninstallString -replace '"', ''
        if (Test-Path $uninstaller) {
            Start-Process -FilePath $uninstaller -ArgumentList "/S" -Wait -NoNewWindow
        }
    }
}

# --- Install main application ---
$installDir = "$env:ProgramFiles\WAIL"
New-Item -ItemType Directory -Force -Path $installDir | Out-Null

Copy-Item "$toolsDir\staged\WAIL.exe" -Destination $installDir -Force
if (Test-Path "$toolsDir\staged\resources") {
    Copy-Item "$toolsDir\staged\resources" -Destination $installDir -Recurse -Force
}
Copy-Item "$toolsDir\staged\opus.dll" -Destination $installDir -Force

# --- Install VST3 plugins ---
New-Item -ItemType Directory -Force -Path $vst3Dir | Out-Null
Copy-Item "$toolsDir\staged\wail-plugin-send.vst3" -Destination $vst3Dir -Recurse -Force
Copy-Item "$toolsDir\staged\wail-plugin-recv.vst3" -Destination $vst3Dir -Recurse -Force

# --- Install CLAP plugins ---
New-Item -ItemType Directory -Force -Path $clapDir | Out-Null
Copy-Item "$toolsDir\staged\wail-plugin-send.clap" -Destination $clapDir -Force
Copy-Item "$toolsDir\staged\wail-plugin-recv.clap" -Destination $clapDir -Force
Copy-Item "$toolsDir\staged\opus.dll" -Destination $clapDir -Force

# --- Shortcuts ---
$exePath = "$installDir\WAIL.exe"
Install-ChocolateyShortcut -ShortcutFilePath "$env:PUBLIC\Desktop\WAIL.lnk" -TargetPath $exePath
$startMenu = "$env:ProgramData\Microsoft\Windows\Start Menu\Programs"
Install-ChocolateyShortcut -ShortcutFilePath "$startMenu\WAIL.lnk" -TargetPath $exePath

# --- Registry: Add/Remove Programs ---
$uninstKey = "HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\WAIL"
New-Item -Path $uninstKey -Force | Out-Null
Set-ItemProperty -Path $uninstKey -Name "DisplayName" -Value "WAIL"
Set-ItemProperty -Path $uninstKey -Name "DisplayVersion" -Value "$env:ChocolateyPackageVersion"
Set-ItemProperty -Path $uninstKey -Name "Publisher" -Value "MostDistant"
Set-ItemProperty -Path $uninstKey -Name "InstallLocation" -Value $installDir
Set-ItemProperty -Path $uninstKey -Name "UninstallString" -Value "choco uninstall wail -y"
Set-ItemProperty -Path $uninstKey -Name "DisplayIcon" -Value "$exePath,0"
Set-ItemProperty -Path $uninstKey -Name "NoModify" -Value 1 -Type DWord
Set-ItemProperty -Path $uninstKey -Name "NoRepair" -Value 1 -Type DWord

# --- Registry: Plugin paths (for uninstall tracking) ---
$pluginKey = "HKLM:\Software\WAIL\PluginPaths"
New-Item -Path $pluginKey -Force | Out-Null
Set-ItemProperty -Path $pluginKey -Name "VST3Dir" -Value $vst3Dir
Set-ItemProperty -Path $pluginKey -Name "CLAPDir" -Value $clapDir
Set-ItemProperty -Path $pluginKey -Name "InstalledVST3" -Value 1 -Type DWord
Set-ItemProperty -Path $pluginKey -Name "InstalledCLAP" -Value 1 -Type DWord

Write-Host "WAIL installed to $installDir"
Write-Host "VST3 plugins installed to $vst3Dir"
Write-Host "CLAP plugins installed to $clapDir"
