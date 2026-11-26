$ErrorActionPreference = 'Stop'
$root = $env:NOTEBOOK_ROOT
$proof = Join-Path $root 'proof'
$project = Join-Path $root 'project'
$userData = Join-Path $root 'profile'
$notebook = Join-Path $project 'Signal study.ipynb'
$zed = $env:ZED_EXE
$log = Join-Path $proof 'drive.txt'

Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class NotebookWindow {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr window, int command);
}
'@

function Note($text) {
    $line = "$(Get-Date -Format o) $text"
    Write-Host $line
    Add-Content -Path $log -Value $line
}

function Processes-Under($rootId) {
    $all = Get-CimInstance Win32_Process
    $ids = @{ $rootId = $true }
    do {
        $added = $false
        foreach ($process in $all) {
            if ($ids.ContainsKey([int]$process.ParentProcessId) -and -not $ids.ContainsKey([int]$process.ProcessId)) {
                $ids[[int]$process.ProcessId] = $true
                $added = $true
            }
        }
    } while ($added)
    $all | Where-Object { $_.ProcessId -ne $rootId -and $ids.ContainsKey([int]$_.ProcessId) }
}

function Record-Python($label) {
    $found = @(Get-CimInstance Win32_Process | Where-Object { $_.Name -like 'python*' })
    Note "$label python processes: $($found.Count)"
    foreach ($process in $found) {
        Note "  pid=$($process.ProcessId) parent=$($process.ParentProcessId) $($process.CommandLine)"
    }
    ,$found
}

function Start-Zed($label) {
    $arguments = @('--user-data-dir', "`"$userData`"", "`"$project`"", "`"$notebook`"")
    $editor = Start-Process $zed -ArgumentList $arguments -PassThru `
        -RedirectStandardOutput (Join-Path $proof "$label.stdout.log") `
        -RedirectStandardError (Join-Path $proof "$label.stderr.log")
    for ($i = 0; $i -lt 120; $i++) {
        $editor.Refresh()
        if ($editor.HasExited) { throw "Zed exited ($($editor.ExitCode)) before opening a window" }
        if ($editor.MainWindowHandle -ne 0) { break }
        Start-Sleep -Seconds 1
    }
    if ($editor.MainWindowHandle -eq 0) { throw 'No Zed window appeared' }
    Note "$label zed pid=$($editor.Id) window=$($editor.MainWindowHandle)"
    [NotebookWindow]::ShowWindow($editor.MainWindowHandle, 3) | Out-Null
    $editor
}

function Focus($editor) {
    $window = $editor.MainWindowHandle
    for ($i = 0; $i -lt 10; $i++) {
        [NotebookWindow]::SetForegroundWindow($window) | Out-Null
        Start-Sleep -Milliseconds 500
        if ([NotebookWindow]::GetForegroundWindow() -eq $window) { return }
    }
    throw 'Cannot bring the Zed window to the foreground'
}

function Shot($editor, $name) {
    Focus $editor
    Start-Sleep -Milliseconds 500
    $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
    $image = New-Object Drawing.Bitmap($bounds.Width, $bounds.Height)
    $graphics = [Drawing.Graphics]::FromImage($image)
    $graphics.CopyFromScreen($bounds.Left, $bounds.Top, 0, 0, $image.Size)
    $image.Save((Join-Path $proof "$name.png"))
    $graphics.Dispose()
    $image.Dispose()
    Note "screenshot $name"
}

function Keys($editor, $keys, $seconds = 1) {
    Focus $editor
    [System.Windows.Forms.SendKeys]::SendWait($keys)
    Note "keys $keys"
    Start-Sleep -Seconds $seconds
}

function Kernels-Under($editor) {
    @(Processes-Under $editor.Id | Where-Object { $_.CommandLine -like '*ipykernel*' })
}

function Wait-Kernel($editor) {
    for ($i = 0; $i -lt 180; $i++) {
        $kernel = Kernels-Under $editor
        if ($kernel.Count -gt 0) {
            foreach ($process in $kernel) { Note "kernel under zed: pid=$($process.ProcessId) parent=$($process.ParentProcessId) $($process.CommandLine)" }
            return
        }
        Start-Sleep -Seconds 1
    }
    throw 'Zed did not start an ipykernel process'
}

function Quit($editor, $label) {
    Keys $editor '^q' 2
    if (-not $editor.WaitForExit(60000)) {
        Shot $editor "$label-quit-blocked"
        throw 'Zed did not quit'
    }
    Note "$label zed exited"
}

$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
Note "screen $($bounds.Width)x$($bounds.Height)"
Note "notebook sha256 before: $((Get-FileHash $notebook).Hash)"
Record-Python 'before launch' | Out-Null

$editor = Start-Zed 'edit-run'
try {
    Start-Sleep -Seconds 20
    Shot $editor '01-opened'
    Keys $editor '{ENTER}' 5
    Shot $editor '02-trusted'

    Keys $editor '{ENTER}' 2
    Keys $editor '^{HOME}+{END}'
    Keys $editor '# Calibrated signal study' 2
    Keys $editor '^{ENTER}' 2
    Shot $editor '03-heading-edited'

    Keys $editor '{DOWN}'
    Keys $editor '{ENTER}' 2
    Keys $editor '^{HOME}+{END}'
    Keys $editor 'gain = 3' 2
    Shot $editor '04-code-edited'

    Keys $editor '+{ENTER}'
    Wait-Kernel $editor
    Start-Sleep -Seconds 25
    Shot $editor '05-first-cell-ran'

    Keys $editor '^{ENTER}' 45
    Shot $editor '06-chart'

    Keys $editor '^s' 5
    Shot $editor '07-saved'
    Copy-Item $notebook (Join-Path $proof 'saved.ipynb')
    Note "notebook sha256 after save: $((Get-FileHash $notebook).Hash)"
    Record-Python 'after save' | Out-Null

    Keys $editor '^+r' 10
    Shot $editor '08-stopped'
    $remaining = Kernels-Under $editor
    Note "kernels under zed after stop: $($remaining.Count)"
    if ($remaining.Count -ne 0) { throw 'The kernel kept running after Stop' }

    Quit $editor 'edit-run'
} finally {
    if (-not $editor.HasExited) {
        Note 'stopping the first Zed process'
        Stop-Process -Id $editor.Id -Force
    }
}

Start-Sleep -Seconds 5
$afterQuit = Record-Python 'after quit'
if ($afterQuit.Count -ne 0) { throw 'Python still running after Zed quit' }
$savedHash = (Get-FileHash $notebook).Hash

$editor = Start-Zed 'reopen'
try {
    Start-Sleep -Seconds 20
    Shot $editor '09-reopened'
    Keys $editor '{DOWN}{DOWN}' 3
    Shot $editor '10-reopened-chart'
    $running = Record-Python 'after reopen'
    if ($running.Count -ne 0) { throw 'Reopening the notebook started Python' }
    if ((Get-FileHash $notebook).Hash -ne $savedHash) { throw 'Reopening changed the saved notebook' }
    Note 'reopen kept the saved file unchanged'
    Quit $editor 'reopen'
} finally {
    if (-not $editor.HasExited) {
        Note 'stopping the reopened Zed process'
        Stop-Process -Id $editor.Id -Force
    }
}
