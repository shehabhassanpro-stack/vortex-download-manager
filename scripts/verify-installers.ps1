# Vortex Download Manager — Automated Windows Installer Builder & < 15 MB Gate (P1-T37)
param(
    [switch]$BundleOnly
)
$ErrorActionPreference = "Stop"

$MaxBridgeBytes = 1000000      # < 1 MB strict thin-bridge budget
$MaxInstallerBytes = 15728640  # < 15 MB (15 MiB) strict installer budget

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
Push-Location $RepoRoot

try {
    Write-Host "==> [1/4] Building release vortex-bridge.exe (< 1 MB)..."
    cargo build --release -p vortex-bridge
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release -p vortex-bridge failed" }

    $BridgeExe = Join-Path $RepoRoot "target\release\vortex-bridge.exe"
    if (-not (Test-Path $BridgeExe)) { throw "Missing $BridgeExe" }
    $BridgeSize = (Get-Item $BridgeExe).Length
    Write-Host ("    vortex-bridge.exe size: {0:N0} bytes ({1:N2} KiB)" -f $BridgeSize, ($BridgeSize / 1024.0))
    if ($BridgeSize -ge $MaxBridgeBytes) {
        throw "BUDGET VIOLATION: vortex-bridge.exe ($BridgeSize bytes) >= 1,000,000 bytes!"
    }

    Write-Host "==> [2/4] Building Windows Per-User NSIS (.exe) & WiX (.msi) installers via Tauri v2..."
    $DesktopExe = Join-Path $RepoRoot "target\release\vortex-desktop.exe"
    if ($BundleOnly -and (Test-Path $DesktopExe)) {
        pnpm --dir apps/desktop/src-tauri exec tauri bundle --bundles nsis msi
    } else {
        pnpm --dir apps/desktop/src-tauri exec tauri build --bundles nsis msi
    }
    if ($LASTEXITCODE -ne 0) { throw "tauri installer build failed" }

    Write-Host "==> [3/4] Configuring & Auditing Per-User (Zero-Admin UAC) + Native Host Manifests..."
    $TauriConf = Get-Content "apps\desktop\src-tauri\tauri.conf.json" -Raw | ConvertFrom-Json
    if ($TauriConf.bundle.windows.nsis.installMode -ne "currentUser") {
        throw "NSIS installMode MUST be 'currentUser' (no Admin UAC)!"
    }
    $NativeManifest = Get-Content "installer\windows\com.vortex.bridge.json" -Raw | ConvertFrom-Json
    if ($NativeManifest.name -ne "com.vortex.bridge" -or $NativeManifest.type -ne "stdio") {
        throw "Invalid com.vortex.bridge.json manifest!"
    }

    $NsisFiles = @(Get-ChildItem "target\release\bundle\nsis\*.exe" -ErrorAction SilentlyContinue)
    $MsiFiles  = @(Get-ChildItem "target\release\bundle\msi\*.msi" -ErrorAction SilentlyContinue)
    if ($NsisFiles.Count -eq 0) { throw "No NSIS .exe installer generated under target\release\bundle\nsis\" }
    if ($MsiFiles.Count -eq 0)  { throw "No WiX .msi installer generated under target\release\bundle\msi\" }

    # Stamp & verify MSI SummaryInformation PID_WORDCOUNT bit 0x0008 (InstallPrivileges="limited" -> No UAC prompt)
    # and Property.ALLUSERS = '2' + MSIINSTALLPERUSER = '1' (Windows Installer 5.0 Per-User Default)
    $MsiPath = $MsiFiles[0].FullName
    $Installer = New-Object -ComObject WindowsInstaller.Installer
    $Db = $Installer.GetType().InvokeMember("OpenDatabase", "InvokeMethod", $null, $Installer, @($MsiPath, 1))

    $UpdateAllUsers = $Db.GetType().InvokeMember("OpenView", "InvokeMethod", $null, $Db, @("UPDATE Property SET Value = '2' WHERE Property = 'ALLUSERS'"))
    $null = $UpdateAllUsers.GetType().InvokeMember("Execute", "InvokeMethod", $null, $UpdateAllUsers, $null)
    $null = $UpdateAllUsers.GetType().InvokeMember("Close", "InvokeMethod", $null, $UpdateAllUsers, $null)
    [System.Runtime.InteropServices.Marshal]::FinalReleaseComObject($UpdateAllUsers) | Out-Null

    $SumInfo = $Db.GetType().InvokeMember("SummaryInformation", "GetProperty", $null, $Db, @(2))
    $WordCount = [int]$SumInfo.GetType().InvokeMember("Property", "GetProperty", $null, $SumInfo, @(15))
    $NewWordCount = $WordCount -bor 8
    $SumInfo.GetType().InvokeMember("Property", "SetProperty", $null, $SumInfo, @(15, $NewWordCount))
    $null = $SumInfo.GetType().InvokeMember("Persist", "InvokeMethod", $null, $SumInfo, $null)
    [System.Runtime.InteropServices.Marshal]::FinalReleaseComObject($SumInfo) | Out-Null

    $null = $Db.GetType().InvokeMember("Commit", "InvokeMethod", $null, $Db, $null)
    [System.Runtime.InteropServices.Marshal]::FinalReleaseComObject($Db) | Out-Null
    [GC]::Collect()
    [GC]::WaitForPendingFinalizers()

    # Re-open MSI read-only and verify ALLUSERS=2, MSIINSTALLPERUSER=1, PID_WORDCOUNT bit 8, and Brave HKCU registry row!
    $DbRo = $Installer.GetType().InvokeMember("OpenDatabase", "InvokeMethod", $null, $Installer, @($MsiPath, 0))
    function Get-MsiScalar([string]$Sql) {
        $V = $DbRo.GetType().InvokeMember("OpenView", "InvokeMethod", $null, $DbRo, @($Sql))
        $null = $V.GetType().InvokeMember("Execute", "InvokeMethod", $null, $V, $null)
        $R = $V.GetType().InvokeMember("Fetch", "InvokeMethod", $null, $V, $null)
        $Val = $null
        if ($null -ne $R) {
            $Val = $R.GetType().InvokeMember("StringData", "GetProperty", $null, $R, @(1))
            [System.Runtime.InteropServices.Marshal]::FinalReleaseComObject($R) | Out-Null
        }
        $null = $V.GetType().InvokeMember("Close", "InvokeMethod", $null, $V, $null)
        [System.Runtime.InteropServices.Marshal]::FinalReleaseComObject($V) | Out-Null
        return $Val
    }
    $AllUsersRo  = Get-MsiScalar 'SELECT `Value` FROM `Property` WHERE `Property` = ''ALLUSERS'''
    $PerUserRo   = Get-MsiScalar 'SELECT `Value` FROM `Property` WHERE `Property` = ''MSIINSTALLPERUSER'''
    $BraveRegKey = Get-MsiScalar 'SELECT `Key` FROM `Registry` WHERE `Key` = ''Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.vortex.bridge'''
    $SumRo       = $DbRo.GetType().InvokeMember("SummaryInformation", "GetProperty", $null, $DbRo, @(0))
    $WcRo        = [int]$SumRo.GetType().InvokeMember("Property", "GetProperty", $null, $SumRo, @(15))

    if ($AllUsersRo -ne "2" -or $PerUserRo -ne "1" -or (($WcRo -band 8) -eq 0)) {
        throw "MSI Per-User No-UAC verification failed: ALLUSERS=$AllUsersRo MSIINSTALLPERUSER=$PerUserRo WordCount=$WcRo"
    }
    if ([string]::IsNullOrEmpty($BraveRegKey)) {
        throw "MSI Registry table missing Brave NativeMessagingHosts HKCU entry!"
    }
    [System.Runtime.InteropServices.Marshal]::FinalReleaseComObject($SumRo) | Out-Null
    [System.Runtime.InteropServices.Marshal]::FinalReleaseComObject($DbRo) | Out-Null
    [System.Runtime.InteropServices.Marshal]::FinalReleaseComObject($Installer) | Out-Null
    [GC]::Collect()
    [GC]::WaitForPendingFinalizers()
    Write-Host "    MSI binary verified: ALLUSERS=2, MSIINSTALLPERUSER=1, PID_WORDCOUNT=$WcRo (No-Admin UAC), HKCU NativeHost=OK!"

    Write-Host "==> [4/4] Enforcing < 15 MB (15,728,640 bytes) Size Gate on NSIS & MSI bundles..."
    foreach ($f in ($NsisFiles + $MsiFiles)) {
        $f.Refresh()
        $mb = $f.Length / (1024.0 * 1024.0)
        Write-Host ("    Bundle: {0} -> {1:N0} bytes ({2:N2} MiB)" -f $f.Name, $f.Length, $mb)
        if ($f.Length -ge $MaxInstallerBytes) {
            throw ("BUDGET VIOLATION: Installer {0} ({1} bytes / {2:N2} MiB) exceeded 15 MB limit!" -f $f.Name, $f.Length, $mb)
        }
    }

    Write-Host "installer-gate: OK (NSIS + MSI per-user No-Admin bundles built, Native Hosts wired, all < 15 MB!)"
}
finally {
    Pop-Location
}
