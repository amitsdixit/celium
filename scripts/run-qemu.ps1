# scripts/run-qemu.ps1 — Boot Celium in QEMU+OVMF (Week-6).
#
# What it does
#   1. Builds celloader.efi (`--features real-handoff`) and the
#      celhyper kernel ELF.
#   2. Lays them out under build/esp/ as a synthetic EFI System
#      Partition tree (BOOTX64.EFI + EFI/CELIUM/CELHYPER.ELF).
#   3. Boots qemu-system-x86_64 against OVMF in UEFI mode with the ESP
#      tree exposed via -drive format=raw,file=fat:rw:build/esp.
#   4. Wires QEMU's debug console (port 0xE9) to a file and COM1 to a
#      second file, then matches both for the success markers:
#        * "Celium Guest Alive!"      — emitted by guest itself on E9
#        * "GUEST OK"                 — emitted by dispatcher on COM1
#        * "vmlaunch deferred"        — dev-box accept (no VT-x exposed)
#
# Required
#   * qemu-system-x86_64 on PATH (or set $env:QEMU)
#   * OVMF firmware. The script auto-discovers common install paths;
#     pass -OvmfPath or set $env:OVMF to override.
#
# Common usage
#   pwsh ./scripts/run-qemu.ps1                        # autodetect OVMF, TCG
#   pwsh ./scripts/run-qemu.ps1 -Accel kvm             # Linux/WSL2 with /dev/kvm
#   pwsh ./scripts/run-qemu.ps1 -NoBuild               # re-run without rebuild
#   pwsh ./scripts/run-qemu.ps1 -Verbose               # echo full logs

[CmdletBinding()]
param(
    [string] $OvmfPath = $env:OVMF,
    [ValidateSet('whpx', 'kvm', 'tcg')]
    [string] $Accel = 'tcg',
    [string] $Qemu  = $(if ($env:QEMU) { $env:QEMU } else { 'qemu-system-x86_64' }),
    [int]    $TimeoutSeconds = 60,
    [switch] $NoBuild,
    [switch] $AcceptDeferred
)

$ErrorActionPreference = 'Stop'

function Write-Banner($text, $color = 'Cyan') {
    Write-Host ''
    Write-Host ('=' * 72) -ForegroundColor $color
    Write-Host (" $text") -ForegroundColor $color
    Write-Host ('=' * 72) -ForegroundColor $color
}

# ---- 0. Auto-discover OVMF -------------------------------------------------
if (-not $OvmfPath) {
    $candidates = @(
        # MSYS2/scoop installs.
        "$env:ProgramFiles\qemu\share\edk2-x86_64-code.fd",
        "$env:ProgramFiles\qemu\share\OVMF.fd",
        "$env:USERPROFILE\scoop\apps\qemu\current\share\edk2-x86_64-code.fd",
        # Linux installs (WSL).
        '/usr/share/OVMF/OVMF_CODE.fd',
        '/usr/share/edk2-ovmf/OVMF_CODE.fd',
        '/usr/share/qemu/OVMF.fd'
    )
    $OvmfPath = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $OvmfPath -or -not (Test-Path $OvmfPath)) {
    Write-Error 'OVMF firmware not found. Pass -OvmfPath or set $env:OVMF.'
}
if (-not (Get-Command $Qemu -ErrorAction SilentlyContinue)) {
    Write-Error "QEMU binary '$Qemu' not on PATH. Pass -Qemu or set `$env:QEMU."
}

$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

# Prepend MinGW so the GNU toolchain finds dlltool.exe (per windows-rust.md).
$env:PATH = 'C:\Users\amdix\scoop\apps\mingw\current\bin;' + $env:PATH
$env:CARGO_INCREMENTAL = '0'

# ---- 1. Build artifacts ----------------------------------------------------
$celloader = Join-Path $repo 'bootloader/celloader/target/x86_64-unknown-uefi/release/celloader.efi'
$celhyper  = Join-Path $repo 'crates/celhyper/target/x86_64-unknown-none/release/celhyper'

if (-not $NoBuild) {
    Write-Banner 'building celloader (real-handoff)'
    Push-Location bootloader/celloader
    cargo build --release --features real-handoff
    Pop-Location

    Write-Banner 'building celhyper kernel'
    Push-Location crates/celhyper
    cargo build --release
    Pop-Location
}

if (-not (Test-Path $celloader)) { Write-Error "celloader.efi missing: $celloader" }
if (-not (Test-Path $celhyper))  { Write-Error "celhyper kernel missing: $celhyper" }

# ---- 2. Lay out ESP --------------------------------------------------------
$esp        = Join-Path $repo 'build/esp'
$espBoot    = Join-Path $esp 'EFI/BOOT'
$espCelium  = Join-Path $esp 'EFI/CELIUM'
New-Item -Force -ItemType Directory -Path $espBoot   | Out-Null
New-Item -Force -ItemType Directory -Path $espCelium | Out-Null

Copy-Item -Force $celloader (Join-Path $espBoot   'BOOTX64.EFI')
Copy-Item -Force $celhyper  (Join-Path $espCelium 'CELHYPER.ELF')

Write-Banner "ESP staged at $esp"
Get-ChildItem -Recurse $esp |
    Select-Object @{n='Path';e={$_.FullName.Substring($repo.Length + 1)}}, Length |
    Format-Table -AutoSize

# ---- 3. Boot QEMU ----------------------------------------------------------
$accelArg = switch ($Accel) {
    'whpx' { 'whpx,kernel-irqchip=off' }
    'kvm'  { 'kvm' }
    'tcg'  { 'tcg' }
}
# `+vmx` is honoured under TCG (always) and KVM (with host nested=1); WHPX
# does not yet expose it to guests.
$cpuModel = if ($Accel -eq 'kvm') { 'host,+vmx' } else { 'max,+vmx' }

$debugconLog = Join-Path $repo 'build/debugcon.log'
$com1Log     = Join-Path $repo 'build/com1.log'
Remove-Item $debugconLog, $com1Log -ErrorAction SilentlyContinue

$qemuArgs = @(
    '-machine', 'q35',
    '-accel',   $accelArg,
    '-cpu',     $cpuModel,
    '-m',       '512',
    '-bios',    $OvmfPath,
    '-drive',   "format=raw,file=fat:rw:$esp",
    '-debugcon', "file:$debugconLog",
    '-serial',   "file:$com1Log",
    '-no-reboot',
    '-display',  'none'
)

Write-Banner "launching QEMU ($Accel / $cpuModel)"
Write-Host ($Qemu + ' ' + ($qemuArgs -join ' '))

$process = Start-Process -FilePath $Qemu -ArgumentList $qemuArgs `
    -PassThru -NoNewWindow -RedirectStandardOutput (Join-Path $repo 'build/qemu.stdout.log') `
    -RedirectStandardError  (Join-Path $repo 'build/qemu.stderr.log')

if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
    Write-Warning "QEMU did not exit within $TimeoutSeconds s, killing."
    $process | Stop-Process -Force
}

# ---- 4. Result -------------------------------------------------------------
$debugcon = Get-Content -Raw -ErrorAction SilentlyContinue $debugconLog
$com1     = Get-Content -Raw -ErrorAction SilentlyContinue $com1Log

if ($VerbosePreference -ne 'SilentlyContinue' -or $env:CELIUM_VERBOSE) {
    Write-Banner 'QEMU debug console (port 0xE9)' Yellow
    Write-Host $debugcon
    Write-Banner 'COM1 (kernel log)' Yellow
    Write-Host $com1
}

$liveSuccess = ($debugcon -and $debugcon.Contains('Celium Guest Alive!')) `
            -or ($com1     -and $com1.Contains('GUEST OK'))
$deferred    = ($com1 -and $com1.Contains('vmlaunch deferred'))
$multiVm     = ($com1 -and $com1.Contains('vm_a_id') -and $com1.Contains('vm_b_id'))
$bringUpDone = ($com1 -and $com1.Contains('bring_up complete'))

if ($multiVm) {
    Write-Host '   multi-VM bring-up: vm_a_id and vm_b_id both observed on COM1' -ForegroundColor Cyan
}
if ($bringUpDone) {
    Write-Host '   bring_up complete: control loop returned cleanly' -ForegroundColor Cyan
}

if ($liveSuccess) {
    Write-Banner 'PASS — guest emitted the success marker' Green
    exit 0
} elseif ($deferred -and $AcceptDeferred) {
    if ($multiVm -and $bringUpDone) {
        Write-Banner 'PASS (deferred, multi-VM) — both VMs reached terminal state' Yellow
    } else {
        Write-Banner 'PASS (deferred) — kernel reached vmlaunch on a CPU without VT-x' Yellow
    }
    Write-Host '   (rerun on KVM/nested=1 or real hardware to observe live guest)'
    exit 0
} else {
    Write-Banner 'FAIL — no success marker; printing tail of logs' Red
    Write-Host '----- COM1 (kernel log) -----'
    Write-Host ($com1     | Out-String)
    Write-Host '----- debugcon (port 0xE9) -----'
    Write-Host ($debugcon | Out-String)
    exit 1
}
