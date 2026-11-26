$ErrorActionPreference = 'Stop'
$root = $env:NOTEBOOK_ROOT
$proof = Join-Path $root 'proof'
$project = Join-Path $root 'project'
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
    [DllImport("user32.dll")] public static extern void keybd_event(byte key, byte scan, uint flags, UIntPtr extra);
}
'@

function Note($text) {
    $line = "$(Get-Date -Format o) $text"
    Write-Host $line
    Add-Content -Path $log -Value $line
}

function Start-Zed($label) {
    $arguments = @('--user-data-dir', "`"$userData`"", "`"$project`"", "`"$notebook`"")
    $editor = Start-Process $zed -ArgumentList $arguments -PassThru
    for ($i = 0; $i -lt 120; $i++) {
        $editor.Refresh()
        if ($editor.HasExited) { throw "Zed exited ($($editor.ExitCode)) before opening a window" }
        if ($editor.MainWindowHandle -ne 0) { break }
        Start-Sleep -Seconds 1
    }
    if ($editor.MainWindowHandle -eq 0) { throw 'No Zed window appeared' }
    Start-Sleep -Seconds 5
    $editor.Refresh()
    Note "$label zed pid=$($editor.Id) window=$($editor.MainWindowHandle) title=$($editor.MainWindowTitle)"
    Focus $editor
    [NotebookWindow]::ShowWindow($editor.MainWindowHandle, 3) | Out-Null
    $editor
}

function Focus($editor) {
    for ($i = 0; $i -lt 10; $i++) {
        $editor.Refresh()
        $window = $editor.MainWindowHandle
        if ([NotebookWindow]::GetForegroundWindow() -eq $window) { return }
        # Windows only lets a process take the foreground after recent input, so press and release Alt first.
        [NotebookWindow]::keybd_event(0x12, 0, 0, [UIntPtr]::Zero)
        [NotebookWindow]::keybd_event(0x12, 0, 2, [UIntPtr]::Zero)
        [NotebookWindow]::SetForegroundWindow($window) | Out-Null
        Start-Sleep -Seconds 1
    }
    Note "foreground window $([NotebookWindow]::GetForegroundWindow()) is not Zed window $($editor.MainWindowHandle)"
    Screen 'focus-failed'
    throw 'Cannot bring the Zed window to the foreground'
}

function Screen($name) {
    $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
    $image = New-Object Drawing.Bitmap($bounds.Width, $bounds.Height)
    $graphics = [Drawing.Graphics]::FromImage($image)
    $graphics.CopyFromScreen($bounds.Left, $bounds.Top, 0, 0, $image.Size)
    $image.Save((Join-Path $proof "$name.png"))
    $graphics.Dispose()
    $image.Dispose()
    Note "screenshot $name"
}

function Shot($editor, $name) {
    Focus $editor
    Start-Sleep -Milliseconds 500
    Screen $name
}

function Keys($editor, $keys, $seconds = 1) {
    Focus $editor
    [System.Windows.Forms.SendKeys]::SendWait($keys)
    Note "keys $keys"
    Start-Sleep -Seconds $seconds
}

function Quit($editor, $label) {
    Keys $editor '^q' 2
    if (-not $editor.WaitForExit(60000)) {
        Shot $editor "$label-quit-blocked"
        throw 'Zed did not quit'
    }
    Note "$label zed exited"
}

function Save-State($editor, $label, $chord = '^s') {
    Keys $editor $chord 1
    Copy-Item $notebook (Join-Path $proof "$label.ipynb")
    Shot $editor $label
}

function Headless-Profile($path) {
    $config = New-Item -ItemType Directory -Force (Join-Path $path 'config')
    '{"enable_language_server":false,"show_completions_on_input":false,"auto_update":false,"telemetry":{"diagnostics":false,"metrics":false}}' |
        Set-Content -Encoding utf8NoBOM (Join-Path $config 'settings.json')
}

$trace = 'notebook-started-python'
Register-CimIndicationEvent -Query "SELECT * FROM Win32_ProcessStartTrace WHERE ProcessName = 'python.exe' OR ProcessName = 'pythonw.exe' OR ProcessName = 'python3.exe'" -SourceIdentifier $trace | Out-Null
try {
    foreach ($case in @('Empty', 'Rich')) {
        $notebook = Join-Path $project "$case.ipynb"
        foreach ($phase in @('edit', 'reopen')) {
            $userData = Join-Path $root "profile-$case-$phase"
            Headless-Profile $userData
            $editor = Start-Zed "$case-$phase"
            try {
                Start-Sleep -Seconds 12
                Shot $editor "$case-$phase-prompt"
                Keys $editor '^+%s' 2
                Shot $editor "$case-$phase-restricted"
                if ($phase -eq 'reopen') {
                    Copy-Item $notebook (Join-Path $proof "$($case.ToLower())-reopened.ipynb")
                } elseif ($case -eq 'Empty') {
                    Keys $editor '^+m'
                    Keys $editor '# Windows structure'
                    Keys $editor '{ESC}'
                    Keys $editor '^m'
                    Keys $editor 'value = 17'
                    Keys $editor '{ESC}'
                    Save-State $editor 'empty-added'
                    Keys $editor '%{UP}'
                    Save-State $editor 'empty-up'
                    Keys $editor '%{DOWN}'
                    Save-State $editor 'empty-down'
                    Keys $editor '{BACKSPACE}' 0
                    Save-State $editor 'empty-deleted'
                    Keys $editor '{ENTER}'
                    Keys $editor '^{END} edited'
                    Keys $editor '{ESC}'
                    Keys $editor '^z' 0
                    Save-State $editor 'empty-undone'
                    Keys $editor '^y' 0
                    Save-State $editor 'empty-redone'
                    Keys $editor '^z' 0
                    Save-State $editor 'empty-final' '^ks'
                } else {
                    Keys $editor '%{DOWN}'
                    Save-State $editor 'rich-moved'
                    Keys $editor '{BACKSPACE}' 0
                    Save-State $editor 'rich-deleted'
                    Keys $editor '^z' 0
                    Save-State $editor 'rich-undone' '^ks'
                }
                Quit $editor "$case-$phase"
            } finally {
                if (-not $editor.HasExited) { Stop-Process -Id $editor.Id -Force }
            }
        }
    }
    Start-Sleep -Seconds 2
    $python = @(Get-Event -SourceIdentifier $trace -ErrorAction SilentlyContinue)
    foreach ($event in $python) {
        $process = $event.SourceEventArgs.NewEvent
        Note "Unexpected Python process $($process.ProcessName), PID $($process.ProcessID), parent $($process.ParentProcessID)"
    }
    if ($python.Count -ne 0) { throw 'Python started during passive authoring' }
    Note 'No Python process start events during either authoring or fresh reopen'
} finally {
    Unregister-Event -SourceIdentifier $trace
}
