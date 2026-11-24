# Memory baseline for the memory and thumbnail plan (docs/LumaFiles-内存与缩略图优化方案.md §2 O0).
#
# Samples the process once a second (private bytes, working set, handles, GDI and USER
# objects) while each scenario drives the app through its steps, and writes one CSV per
# run plus a JSON summary (peak and stable value per run).
#
#   pwsh -File scripts/measure-memory.ps1 -Setup          # generate the test folders
#   pwsh -File scripts/measure-memory.ps1 -Scenario cold  # one scenario, 3 runs
#   pwsh -File scripts/measure-memory.ps1 -All            # every scenario
#
# Everything it writes lives under target\tmp\perf. The app runs from a hard link to
# target\release\LumaFiles.exe with a copy of a clean portable profile, so the real
# userdata is never touched and every run starts from the same state.

[CmdletBinding()]
param(
    [switch]$Setup,
    [string]$Scenario = "",
    [switch]$All,
    [int]$Runs = 3,
    [string]$Root = "",
    [switch]$Screenshots
)

$ErrorActionPreference = 'Stop'

if (-not $Root) {
    $Root = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\tmp\perf'
}
$AppSource = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release\LumaFiles.exe'
$UserData = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release\userdata'

$DataDir = Join-Path $Root 'data'
$ProfileDir = Join-Path $Root 'profile'
$RunsDir = Join-Path $Root 'runs'
$Files100k = Join-Path $DataDir 'files100k'
$Photos10k = Join-Path $DataDir 'photos10k'
$Folders100k = Join-Path $DataDir 'folders100k'

# --- Win32 helpers -------------------------------------------------------
Add-Type -AssemblyName System.Windows.Forms
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class PerfWin32
{
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }

    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint attachTo, uint attachFrom, bool attach);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern IntPtr SetFocus(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindowAsync(IntPtr hWnd, int cmd);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern uint GetGuiResources(IntPtr hProcess, uint flags);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, int dx, int dy, int data, UIntPtr extra);

    public const uint MOUSEEVENTF_WHEEL = 0x0800;
    public const uint MOUSEEVENTF_LEFTDOWN = 0x0002;
    public const uint MOUSEEVENTF_LEFTUP = 0x0004;
}
'@

function Get-AppProcess {
    param([System.Diagnostics.Process]$Process, [string]$ExePath)
    # The launcher may hand the path to an earlier instance and exit, so find the
    # process by its executable rather than trusting the one we started.
    $name = [IO.Path]::GetFileNameWithoutExtension($ExePath)
    return Get-Process -Name $name -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $ExePath } |
        Select-Object -First 1
}

function Start-App {
    param([string]$ExePath, [string]$Arguments = "")
    if ($Arguments) {
        return Start-Process -FilePath $ExePath -ArgumentList $Arguments -PassThru
    }
    return Start-Process -FilePath $ExePath -PassThru
}

function Wait-Window {
    param([string]$ExePath, [int]$TimeoutSec = 30)
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        $process = Get-AppProcess -Process $null -ExePath $ExePath
        if ($process -and $process.MainWindowHandle -ne 0) {
            Start-Sleep -Milliseconds 800
            return $process
        }
        Start-Sleep -Milliseconds 250
    }
    throw "no window from $ExePath within $TimeoutSec s"
}

function Focus-App {
    param([System.Diagnostics.Process]$Process)
    if ($Process.MainWindowHandle -eq 0) { return }
    $handle = $Process.MainWindowHandle
    [PerfWin32]::ShowWindowAsync($handle, 9) | Out-Null
    Start-Sleep -Milliseconds 150

    $shell = New-Object -ComObject WScript.Shell
    for ($attempt = 0; $attempt -lt 5; $attempt++) {
        if ($shell.AppActivate($Process.Id)) { break }
        Start-Sleep -Milliseconds 200
    }
    [PerfWin32]::BringWindowToTop($handle) | Out-Null
    [PerfWin32]::SetForegroundWindow($handle) | Out-Null
    Start-Sleep -Milliseconds 200

    # A process that is not the foreground one may not set the foreground: attach
    # to the thread that holds it, which is the documented way around that.
    if ([PerfWin32]::GetForegroundWindow() -ne $handle) {
        $foreground = [PerfWin32]::GetForegroundWindow()
        $foregroundProcess = 0
        $foregroundThread = [PerfWin32]::GetWindowThreadProcessId($foreground, [ref]$foregroundProcess)
        $self = [PerfWin32]::GetCurrentThreadId()
        if ($foregroundThread -ne 0) {
            [PerfWin32]::AttachThreadInput($self, $foregroundThread, $true) | Out-Null
            [PerfWin32]::SetForegroundWindow($handle) | Out-Null
            [PerfWin32]::SetFocus($handle) | Out-Null
            [PerfWin32]::AttachThreadInput($self, $foregroundThread, $false) | Out-Null
        }
        Start-Sleep -Milliseconds 200
    }

    # Last resort, and the one that always works: a click on the window activates it.
    if ([PerfWin32]::GetForegroundWindow() -ne $handle) {
        Move-Pointer -Process $Process -FractionX 0.5 -FractionY 0.5
        [PerfWin32]::mouse_event([PerfWin32]::MOUSEEVENTF_LEFTDOWN, 0, 0, 0, [UIntPtr]::Zero)
        [PerfWin32]::mouse_event([PerfWin32]::MOUSEEVENTF_LEFTUP, 0, 0, 0, [UIntPtr]::Zero)
        Start-Sleep -Milliseconds 300
    }

    if ([PerfWin32]::GetForegroundWindow() -ne $handle) {
        Write-Warning "the window could not be activated; keys may go nowhere"
    }
    Start-Sleep -Milliseconds 200
}

function Send-Keys {
    param([string]$Keys, [int]$DelayMs = 120)
    [System.Windows.Forms.SendKeys]::SendWait($Keys)
    Start-Sleep -Milliseconds $DelayMs
}

function Get-WindowRect {
    param([System.Diagnostics.Process]$Process)
    $rect = New-Object PerfWin32+RECT
    [PerfWin32]::GetWindowRect($Process.MainWindowHandle, [ref]$rect) | Out-Null
    return $rect
}

function Move-Pointer {
    param([System.Diagnostics.Process]$Process, [double]$FractionX = 0.5, [double]$FractionY = 0.5)
    $rect = Get-WindowRect -Process $Process
    $x = [int]($rect.Left + ($rect.Right - $rect.Left) * $FractionX)
    $y = [int]($rect.Top + ($rect.Bottom - $rect.Top) * $FractionY)
    [PerfWin32]::SetCursorPos($x, $y) | Out-Null
    Start-Sleep -Milliseconds 80
}

function Click-App {
    param([System.Diagnostics.Process]$Process, [double]$FractionX = 0.5, [double]$FractionY = 0.5)
    Move-Pointer -Process $Process -FractionX $FractionX -FractionY $FractionY
    [PerfWin32]::mouse_event([PerfWin32]::MOUSEEVENTF_LEFTDOWN, 0, 0, 0, [UIntPtr]::Zero)
    [PerfWin32]::mouse_event([PerfWin32]::MOUSEEVENTF_LEFTUP, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 150
}

function Scroll-App {
    param([System.Diagnostics.Process]$Process, [int]$Notches, [int]$DelayMs = 3, [double]$FractionX = 0.5, [double]$FractionY = 0.5)
    Move-Pointer -Process $Process -FractionX $FractionX -FractionY $FractionY
    for ($i = 0; $i -lt $Notches; $i++) {
        [PerfWin32]::mouse_event([PerfWin32]::MOUSEEVENTF_WHEEL, 0, 0, -120, [UIntPtr]::Zero)
        if ($DelayMs -gt 0) { Start-Sleep -Milliseconds $DelayMs }
    }
}

function Open-Path {
    param([string]$ExePath, [string]$Path)
    $process = Start-Process -FilePath $ExePath -ArgumentList ('"' + $Path + '"') -PassThru
    $null = $process.WaitForExit(10000)
    Start-Sleep -Milliseconds 600
}

function Save-Screenshot {
    param([System.Diagnostics.Process]$Process, [string]$Path)
    if (-not $Screenshots) { return }
    $rect = Get-WindowRect -Process $Process
    $width = $rect.Right - $rect.Left
    $height = $rect.Bottom - $rect.Top
    if ($width -le 0 -or $height -le 0) { return }
    Add-Type -AssemblyName System.Drawing
    $bitmap = New-Object System.Drawing.Bitmap $width, $height
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.CopyFromScreen($rect.Left, $rect.Top, 0, 0, (New-Object System.Drawing.Size $width, $height))
    $graphics.Dispose()
    $bitmap.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bitmap.Dispose()
    Write-Host "  screenshot: $Path"
}

function Update-IndexStamp {
    # The file header is magic (8 bytes), then the `built` stamp as little-endian
    # seconds. A stamp older than six hours makes the app walk the drives again
    # after loading, which is scenario 2b, not this one.
    param([string]$Path)
    $now = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite)
    try {
        $stream.Position = 8
        $bytes = [BitConverter]::GetBytes([int64]$now)
        $stream.Write($bytes, 0, 8)
    } finally {
        $stream.Dispose()
    }
}

function Get-Sample {
    param([System.Diagnostics.Process]$Process, [double]$Seconds)
    $Process.Refresh()
    $gdi = [PerfWin32]::GetGuiResources($Process.Handle, 0)
    $user = [PerfWin32]::GetGuiResources($Process.Handle, 1)
    return [pscustomobject]@{
        seconds = [math]::Round($Seconds, 1)
        private_mb = [math]::Round($Process.PrivateMemorySize64 / 1MB, 1)
        working_set_mb = [math]::Round($Process.WorkingSet64 / 1MB, 1)
        handles = $Process.HandleCount
        gdi = $gdi
        user = $user
    }
}

function Get-Median {
    param([double[]]$Values)
    if (-not $Values -or $Values.Count -eq 0) { return 0 }
    $sorted = $Values | Sort-Object
    $middle = [int][math]::Floor($sorted.Count / 2)
    if ($sorted.Count % 2 -eq 0) {
        return [math]::Round(($sorted[$middle - 1] + $sorted[$middle]) / 2, 1)
    }
    return $sorted[$middle]
}

# --- the test folders ----------------------------------------------------
function New-TestData {
    New-Item -ItemType Directory -Force -Path $DataDir | Out-Null

    if (-not (Test-Path $Files100k)) {
        Write-Host "creating $Files100k (100,000 files)"
        New-Item -ItemType Directory -Force -Path $Files100k | Out-Null
        $bytes = New-Object byte[] 256
        $watch = [Diagnostics.Stopwatch]::StartNew()
        for ($i = 1; $i -le 100000; $i++) {
            $name = 'f{0:D6}.bin' -f $i
            [IO.File]::WriteAllBytes((Join-Path $Files100k $name), $bytes)
        }
        Write-Host ("  {0:N0} s" -f $watch.Elapsed.TotalSeconds)
    }

    if (-not (Test-Path $Photos10k)) {
        Write-Host "creating $Photos10k (10,000 photos)"
        New-Item -ItemType Directory -Force -Path $Photos10k | Out-Null
        Add-Type -AssemblyName System.Drawing
        $base = Join-Path $DataDir 'photo_base.jpg'
        $bitmap = New-Object System.Drawing.Bitmap 800, 600
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        $brushA = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
            (New-Object System.Drawing.Rectangle 0, 0, 800, 600),
            [System.Drawing.Color]::FromArgb(255, 30, 60, 120),
            [System.Drawing.Color]::FromArgb(255, 200, 120, 40),
            45.0)
        $graphics.FillRectangle($brushA, 0, 0, 800, 600)
        $pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::White), 6.0
        $graphics.DrawEllipse($pen, 220, 140, 360, 360)
        $graphics.Dispose()
        $bitmap.Save($base, [System.Drawing.Imaging.ImageFormat]::Jpeg)
        $bitmap.Dispose()
        $watch = [Diagnostics.Stopwatch]::StartNew()
        for ($i = 1; $i -le 10000; $i++) {
            $name = 'photo_{0:D5}.jpg' -f $i
            Copy-Item -LiteralPath $base -Destination (Join-Path $Photos10k $name)
        }
        Remove-Item -LiteralPath $base
        Write-Host ("  {0:N0} s" -f $watch.Elapsed.TotalSeconds)
    }

    if (-not (Test-Path $Folders100k)) {
        Write-Host "creating $Folders100k (100,000 folders)"
        New-Item -ItemType Directory -Force -Path $Folders100k | Out-Null
        $watch = [Diagnostics.Stopwatch]::StartNew()
        for ($i = 1; $i -le 100000; $i++) {
            $name = 'folder_{0:D6}' -f $i
            [IO.Directory]::CreateDirectory((Join-Path $Folders100k $name)) | Out-Null
        }
        Write-Host ("  {0:N0} s" -f $watch.Elapsed.TotalSeconds)
    }
}

function New-Profile {
    New-Item -ItemType Directory -Force -Path $ProfileDir | Out-Null
    $userData = Join-Path $ProfileDir 'userdata'
    if (Test-Path $userData) { Remove-Item -LiteralPath $userData -Recurse -Force }
    foreach ($sub in 'cache', 'data', 'logs', 'runtime', 'state') {
        New-Item -ItemType Directory -Force -Path (Join-Path $userData $sub) | Out-Null
    }
    $config = @'
schema_version = 1

[storage]
portable_mode = true

[ui]
default_sort = "name"
show_hidden = false
welcome_done = true

[explorer]
restore_tabs = false
synced_panes = false
'@
    $search = @'
[indexing]
source = "prime-files"
'@
    $quicklaunch = @'
[window]
keep_running = true
auto_hide = false
'@
    [IO.File]::WriteAllText((Join-Path $userData 'config.toml'), $config, [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText((Join-Path $userData 'search.toml'), $search, [Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllText((Join-Path $userData 'quicklaunch.toml'), $quicklaunch, [Text.UTF8Encoding]::new($false))
}

function Reset-RunDirectory {
    param([string]$ScenarioName, [int]$Run)
    $runDir = Join-Path $RunsDir ("{0}-{1}" -f $ScenarioName, $Run)
    if (Test-Path $runDir) { Remove-Item -LiteralPath $runDir -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $runDir | Out-Null
    $exe = Join-Path $runDir 'LumaFiles.exe'
    New-Item -ItemType HardLink -Path $exe -Target $AppSource | Out-Null
    Copy-Item -LiteralPath (Join-Path $ProfileDir 'userdata') -Destination (Join-Path $runDir 'userdata') -Recurse
    return @{ Dir = $runDir; Exe = $exe; UserData = (Join-Path $runDir 'userdata') }
}

function Stop-App {
    param([string]$ExePath)
    $process = Get-AppProcess -Process $null -ExePath $ExePath
    if ($process) {
        $null = $process.CloseMainWindow()
        Start-Sleep -Milliseconds 500
        $process = Get-AppProcess -Process $null -ExePath $ExePath
        if ($process) { $process | Stop-Process -Force }
        Start-Sleep -Milliseconds 500
    }
}

# --- scenario drivers ----------------------------------------------------
# Each returns the samples it collected; the caller stops the app afterwards.
function Invoke-Scenario {
    param([string]$Name, [int]$Run)

    $runInfo = Reset-RunDirectory -ScenarioName $Name -Run $Run
    $samples = New-Object 'System.Collections.Generic.List[object]'
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $process = $null
    $done = $false

    try {
        if ($Name -ne 'cold') {
            Write-Host ("  window: " + (Get-AppProcess -Process $null -ExePath $runInfo.Exe).MainWindowTitle)
        }
        switch ($Name) {
            'cold' {
                $process = Start-App -ExePath $runInfo.Exe
                $deadline = (Get-Date).AddSeconds(25)
                while (-not $process.HasExited -and $process.MainWindowHandle -eq 0 -and (Get-Date) -lt $deadline) {
                    $process.Refresh()
                    Start-Sleep -Milliseconds 200
                }
                while ($watch.Elapsed.TotalSeconds -lt 40) {
                    if ($process.HasExited) { break }
                    $samples.Add((Get-Sample -Process $process -Seconds $watch.Elapsed.TotalSeconds))
                    Start-Sleep -Milliseconds 900
                }
            }

            'index' {
                $indexSource = Join-Path $Root 'index-fresh.idx'
                if (-not (Test-Path $indexSource)) {
                    $indexSource = Join-Path $UserData 'cache\everything.idx'
                }
                if (Test-Path $indexSource) {
                    $copied = Join-Path $runInfo.UserData 'cache\everything.idx'
                    Copy-Item -LiteralPath $indexSource -Destination $copied
                    # The fixture ages out after six hours; stamping the copy keeps
                    # this scenario on the load path instead of the rebuild one.
                    Update-IndexStamp -Path $copied
                }
                $process = Start-App -ExePath $runInfo.Exe
                Start-Sleep -Seconds 4
                $process = Wait-Window -ExePath $runInfo.Exe
                Focus-App -Process $process
                Send-Keys -Keys '^+f' -DelayMs 1500
                # The whole-computer search reads the index when it has a query to
                # run, not when the page opens.
                Send-Keys -Keys 'a' -DelayMs 500
                $stableSince = 0.0
                while ($watch.Elapsed.TotalSeconds -lt 240 -and -not $process.HasExited) {
                    $samples.Add((Get-Sample -Process $process -Seconds $watch.Elapsed.TotalSeconds))
                    if ($samples.Count -ge 10) {
                        $window = $samples.GetRange($samples.Count - 10, 10) | ForEach-Object { $_.private_mb }
                        $spread = ($window | Measure-Object -Maximum).Maximum - ($window | Measure-Object -Minimum).Minimum
                        if ($spread -lt [math]::Max(4.0, 0.005 * ($window | Measure-Object -Maximum).Maximum)) {
                            $stableSince += 1.0
                        } else {
                            $stableSince = 0.0
                        }
                        if ($stableSince -ge 10) { $done = $true; break }
                    }
                    Start-Sleep -Milliseconds 900
                }
            }

            'index_stale' {
                # The snapshot with its original stamp: the app loads it and, since
                # it is older than six hours, walks the drives again. This is the
                # state most users are in, and O2 is measured on it.
                $indexSource = Join-Path $UserData 'cache\everything.idx'
                if (Test-Path $indexSource) {
                    Copy-Item -LiteralPath $indexSource -Destination (Join-Path $runInfo.UserData 'cache\everything.idx')
                }
                $process = Start-App -ExePath $runInfo.Exe
                Start-Sleep -Seconds 4
                $process = Wait-Window -ExePath $runInfo.Exe
                Focus-App -Process $process
                Send-Keys -Keys '^+f' -DelayMs 1500
                Send-Keys -Keys 'a' -DelayMs 500
                $peak = 0.0
                $settled = 0
                while ($watch.Elapsed.TotalSeconds -lt 300 -and -not $process.HasExited) {
                    $sample = Get-Sample -Process $process -Seconds $watch.Elapsed.TotalSeconds
                    $samples.Add($sample)
                    if ($sample.private_mb -gt $peak) { $peak = [double]$sample.private_mb }
                    # Loading plus walking raises the peak and then lets it go; once
                    # it has dropped and stayed put the scenario is over.
                    if ($peak -gt 300 -and $sample.private_mb -lt $peak * 0.95) {
                        $settled += 1
                        if ($settled -ge 15) { break }
                    } else {
                        $settled = 0
                    }
                    Start-Sleep -Milliseconds 900
                }
            }

            'bigfolder' {
                $process = Start-App -ExePath $runInfo.Exe -Arguments ('"' + $Files100k + '"')
                $process = Wait-Window -ExePath $runInfo.Exe
                Save-Screenshot -Process $process -Path ((Join-Path $RunsDir ("{0}-{1}-folder.png" -f $Name, $Run)))
                Click-App -Process $process -FractionX 0.6 -FractionY 0.7
                $typed = $false
                while ($watch.Elapsed.TotalSeconds -lt 35 -and -not $process.HasExited) {
                    $samples.Add((Get-Sample -Process $process -Seconds $watch.Elapsed.TotalSeconds))
                    if (-not $typed -and $watch.Elapsed.TotalSeconds -gt 12) {
                        Focus-App -Process $process
                        Send-Keys -Keys 'abcde' -DelayMs 300
                        $typed = $true
                    }
                    Start-Sleep -Milliseconds 900
                }
            }

            'photos' {
                $process = Start-App -ExePath $runInfo.Exe -Arguments ('"' + $Photos10k + '"')
                $process = Wait-Window -ExePath $runInfo.Exe
                Focus-App -Process $process
                Click-App -Process $process -FractionX 0.6 -FractionY 0.7
                for ($i = 0; $i -lt 15; $i++) { Send-Keys -Keys '^=' -DelayMs 90 }
                Start-Sleep -Seconds 1
                Save-Screenshot -Process $process -Path ((Join-Path $RunsDir ("{0}-{1}-zoomed.png" -f $Name, $Run)))
                $scrolled = 0
                while ($watch.Elapsed.TotalSeconds -lt 60 -and -not $process.HasExited) {
                    $samples.Add((Get-Sample -Process $process -Seconds $watch.Elapsed.TotalSeconds))
                    if ($watch.Elapsed.TotalSeconds -gt 8 -and $scrolled -lt 2400) {
                        Scroll-App -Process $process -Notches 40 -DelayMs 3
                        $scrolled += 40
                    }
                    Start-Sleep -Milliseconds 900
                }
            }

            'tabs_switch' {
                # R1 of the first memory review: two tabs each scroll the album,
                # then the first one comes back. What it held while in the
                # background is in the app's log (`trimmed its thumbnails`) and the
                # private memory in the samples.
                $process = Start-App -ExePath $runInfo.Exe -Arguments ('"' + $Photos10k + '"')
                $process = Wait-Window -ExePath $runInfo.Exe
                Focus-App -Process $process
                Click-App -Process $process -FractionX 0.6 -FractionY 0.7
                for ($i = 0; $i -lt 15; $i++) { Send-Keys -Keys '^=' -DelayMs 90 }
                Start-Sleep -Seconds 1
                Save-Screenshot -Process $process -Path ((Join-Path $RunsDir ("{0}-{1}-one.png" -f $Name, $Run)))
                Send-Keys -Keys '^t' -DelayMs 900
                Open-Path -ExePath $runInfo.Exe -Path $Photos10k
                Start-Sleep -Seconds 1
                Click-App -Process $process -FractionX 0.6 -FractionY 0.7
                for ($i = 0; $i -lt 15; $i++) { Send-Keys -Keys '^=' -DelayMs 90 }
                Start-Sleep -Seconds 1
                Save-Screenshot -Process $process -Path ((Join-Path $RunsDir ("{0}-{1}-two.png" -f $Name, $Run)))
                $scrolled = 0
                $switched = $false
                $switchSeconds = 0.0
                while ($watch.Elapsed.TotalSeconds -lt 75 -and -not $process.HasExited) {
                    $sample = Get-Sample -Process $process -Seconds $watch.Elapsed.TotalSeconds
                    $samples.Add($sample)
                    if ($watch.Elapsed.TotalSeconds -gt 8 -and $scrolled -lt 1600) {
                        Scroll-App -Process $process -Notches 20 -DelayMs 3
                        $scrolled += 20
                    } elseif (-not $switched -and $watch.Elapsed.TotalSeconds -gt 30) {
                        # Back to the first tab: the second one should keep one screen.
                        Click-App -Process $process -FractionX 0.25 -FractionY 0.6
                        Send-Keys -Keys '^+{TAB}' -DelayMs 400
                        Click-App -Process $process -FractionX 0.6 -FractionY 0.7
                        $switched = $true
                        $switchSeconds = $watch.Elapsed.TotalSeconds
                        Write-Output ("  switched back at {0:N1} s, private {1:N1} MB" -f $switchSeconds, $sample.private_mb)
                    }
                    Start-Sleep -Milliseconds 900
                }
                Save-Screenshot -Process $process -Path ((Join-Path $RunsDir ("{0}-{1}-back.png" -f $Name, $Run)))
            }

            'dualpane' {
                # Two tabs, each with its own thumbnails cache: the pane chords do
                # not dispatch from a synthetic key press, and a tab owns the same
                # per-view cache a pane does.
                $process = Start-App -ExePath $runInfo.Exe -Arguments ('"' + $Photos10k + '"')
                $process = Wait-Window -ExePath $runInfo.Exe
                Focus-App -Process $process
                Click-App -Process $process -FractionX 0.6 -FractionY 0.7
                for ($i = 0; $i -lt 15; $i++) { Send-Keys -Keys '^=' -DelayMs 90 }
                Start-Sleep -Seconds 1
                Save-Screenshot -Process $process -Path ((Join-Path $RunsDir ("{0}-{1}-one.png" -f $Name, $Run)))
                Send-Keys -Keys '^t' -DelayMs 900
                Open-Path -ExePath $runInfo.Exe -Path $Photos10k
                Start-Sleep -Seconds 1
                Click-App -Process $process -FractionX 0.6 -FractionY 0.7
                for ($i = 0; $i -lt 15; $i++) { Send-Keys -Keys '^=' -DelayMs 90 }
                Start-Sleep -Seconds 1
                Save-Screenshot -Process $process -Path ((Join-Path $RunsDir ("{0}-{1}-two.png" -f $Name, $Run)))
                $scrolled = 0
                while ($watch.Elapsed.TotalSeconds -lt 80 -and -not $process.HasExited) {
                    $samples.Add((Get-Sample -Process $process -Seconds $watch.Elapsed.TotalSeconds))
                    if ($watch.Elapsed.TotalSeconds -gt 10 -and $scrolled -lt 1200) {
                        Scroll-App -Process $process -Notches 20 -DelayMs 3
                        $scrolled += 20
                    } elseif ($scrolled -ge 1200 -and $scrolled -lt 2400) {
                        Click-App -Process $process -FractionX 0.25 -FractionY 0.6
                        Send-Keys -Keys '^+{TAB}' -DelayMs 300
                        Click-App -Process $process -FractionX 0.6 -FractionY 0.7
                        Scroll-App -Process $process -Notches 20 -DelayMs 3
                        $scrolled += 20
                    }
                    Start-Sleep -Milliseconds 900
                }
            }

            'folders' {
                $process = Start-App -ExePath $runInfo.Exe -Arguments ('"' + $Folders100k + '"')
                $process = Wait-Window -ExePath $runInfo.Exe
                Focus-App -Process $process
                Click-App -Process $process -FractionX 0.6 -FractionY 0.7
                for ($i = 0; $i -lt 8; $i++) { Send-Keys -Keys '^-' -DelayMs 120 }
                Start-Sleep -Seconds 1
                Save-Screenshot -Process $process -Path ((Join-Path $RunsDir ("{0}-{1}-list.png" -f $Name, $Run)))
                while ($watch.Elapsed.TotalSeconds -lt 100 -and -not $process.HasExited) {
                    $samples.Add((Get-Sample -Process $process -Seconds $watch.Elapsed.TotalSeconds))
                    if ($watch.Elapsed.TotalSeconds -gt 8) {
                        Scroll-App -Process $process -Notches 60 -DelayMs 2
                    }
                    Start-Sleep -Milliseconds 500
                }
            }

            'background' {
                $process = Start-App -ExePath $runInfo.Exe
                $process = Wait-Window -ExePath $runInfo.Exe
                Focus-App -Process $process
                Send-Keys -Keys '%{F4}' -DelayMs 1500
                $process = Get-AppProcess -Process $null -ExePath $runInfo.Exe
                if (-not $process) { throw 'the process exited when the window was closed' }
                while ($watch.Elapsed.TotalSeconds -lt 620 -and -not $process.HasExited) {
                    $samples.Add((Get-Sample -Process $process -Seconds $watch.Elapsed.TotalSeconds))
                    Start-Sleep -Milliseconds 900
                }
            }

            'openclose' {
                $process = Start-App -ExePath $runInfo.Exe -Arguments ('"' + $Files100k + '"')
                $process = Wait-Window -ExePath $runInfo.Exe
                Focus-App -Process $process
                $cycle = 0
                while ($watch.Elapsed.TotalSeconds -lt 90 -and -not $process.HasExited) {
                    $samples.Add((Get-Sample -Process $process -Seconds $watch.Elapsed.TotalSeconds))
                    if ($cycle -lt 10 -and ($watch.Elapsed.TotalSeconds -gt (6 + $cycle * 7))) {
                        if ($cycle -gt 0) {
                            Focus-App -Process $process
                            Send-Keys -Keys '^w' -DelayMs 500
                        }
                        Open-Path -ExePath $runInfo.Exe -Path $Files100k
                        $cycle += 1
                    }
                    Start-Sleep -Milliseconds 900
                }
                Focus-App -Process $process
                Send-Keys -Keys '^w' -DelayMs 500
                $after = $watch.Elapsed.TotalSeconds
                while ($watch.Elapsed.TotalSeconds -lt ($after + 10) -and -not $process.HasExited) {
                    $samples.Add((Get-Sample -Process $process -Seconds $watch.Elapsed.TotalSeconds))
                    Start-Sleep -Milliseconds 900
                }
            }

            default { throw "unknown scenario $Name" }
        }
    } finally {
        if ($process -and -not $process.HasExited) {
            Save-Screenshot -Process $process -Path ((Join-Path $RunsDir ("{0}-{1}-end.png" -f $Name, $Run)))
            Stop-App -ExePath $runInfo.Exe
        }
    }

    $csv = Join-Path $RunsDir ("{0}-{1}.csv" -f $Name, $Run)
    $samples | Export-Csv -LiteralPath $csv -NoTypeInformation -Encoding utf8

    $private = @($samples | ForEach-Object { [double]$_.private_mb })
    $stableCount = [math]::Min(10, [math]::Max(1, $private.Count - 1))
    $last = $private[($private.Count - $stableCount)..($private.Count - 1)]
    return [pscustomobject]@{
        scenario = $Name
        run = $Run
        samples = $private.Count
        peak_private_mb = [math]::Round((($private | Measure-Object -Maximum).Maximum), 1)
        stable_private_mb = Get-Median -Values $last
        peak_working_set_mb = [math]::Round((($samples | ForEach-Object { [double]$_.working_set_mb } | Measure-Object -Maximum).Maximum), 1)
        peak_handles = ($samples | ForEach-Object { [int]$_.handles } | Measure-Object -Maximum).Maximum
        peak_gdi = ($samples | ForEach-Object { [int]$_.gdi } | Measure-Object -Maximum).Maximum
        peak_user = ($samples | ForEach-Object { [int]$_.user } | Measure-Object -Maximum).Maximum
        csv = $csv
    }
}

# --- service control for the own-index scenario -------------------------
function Set-IndexerService {
    param([bool]$Running)
    $service = Get-Service -Name PrimeFilesIndexer -ErrorAction SilentlyContinue
    if (-not $service) { return $false }
    if ($Running) {
        if ($service.Status -ne 'Running') { Start-Service -Name PrimeFilesIndexer }
    } else {
        if ($service.Status -ne 'Stopped') { Stop-Service -Name PrimeFilesIndexer }
    }
    Start-Sleep -Seconds 2
    return $true
}

# --- entry point ---------------------------------------------------------
New-Item -ItemType Directory -Force -Path $RunsDir | Out-Null

if ($Setup) {
    New-TestData
    New-Profile
    Write-Host "setup done: $Root"
    return
}

if (-not (Test-Path (Join-Path $ProfileDir 'userdata'))) {
    throw "no profile; run -Setup first"
}

$scenarios = if ($All) {
    @('cold', 'index', 'index_stale', 'bigfolder', 'photos', 'dualpane', 'tabs_switch', 'folders', 'background', 'openclose')
} elseif ($Scenario) {
    @($Scenario)
} else {
    throw 'pick -Scenario <name> or -All'
}

$serviceStopped = $false
$results = New-Object 'System.Collections.Generic.List[object]'
try {
    foreach ($name in $scenarios) {
        if ($name -eq 'index' -or $name -eq 'index_stale') {
            $serviceStopped = Set-IndexerService -Running $false
        }
        foreach ($run in 1..$Runs) {
            Write-Host ("scenario {0}, run {1}/{2}" -f $name, $run, $Runs)
            try {
                $results.Add((Invoke-Scenario -Name $name -Run $run))
            } catch {
                Write-Warning ("{0} run {1} failed: {2}" -f $name, $run, $_.Exception.Message)
                $results.Add([pscustomobject]@{
                    scenario = $name
                    run = $run
                    samples = 0
                    peak_private_mb = 0
                    stable_private_mb = 0
                    peak_working_set_mb = 0
                    peak_handles = 0
                    peak_gdi = 0
                    peak_user = 0
                    csv = ""
                    error = $_.Exception.Message
                })
            }
        }
        if (($name -eq 'index' -or $name -eq 'index_stale') -and $serviceStopped) {
            $serviceStopped = -not (Set-IndexerService -Running $true)
        }
    }
} finally {
    if ($serviceStopped) { Set-IndexerService -Running $true | Out-Null }
}

$summary = Join-Path $Root 'summary.json'
$results | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $summary -Encoding utf8
$results | Format-Table -AutoSize | Out-String | Write-Host
Write-Host "summary: $summary"