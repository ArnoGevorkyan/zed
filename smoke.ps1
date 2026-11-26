param(
    [Parameter(Mandatory)][string]$PackageDir,
    [Parameter(Mandatory)][string]$EvidenceDir
)

$ErrorActionPreference = 'Stop'
if ($env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows' -or $env:RUNNER_ARCH -ne 'X64') {
    throw 'This smoke runs only on a standard GitHub-hosted Windows x64 runner'
}
if ($PSVersionTable.PSEdition -eq 'Core') {
    # Windows PowerShell includes the desktop UI Automation assemblies.
    & "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -STA -File $PSCommandPath -PackageDir $PackageDir -EvidenceDir $EvidenceDir
    if ($LASTEXITCODE) { throw "Windows smoke failed ($LASTEXITCODE)" }
    return
}

$PackageDir = (Get-Item -LiteralPath $PackageDir).FullName
$exe = (Get-Item -LiteralPath (Join-Path $PackageDir 'Zed.exe')).FullName
$notebook = (Get-Item -LiteralPath (Join-Path $PackageDir 'Notebook.ipynb')).FullName
$before = (Get-FileHash -LiteralPath $notebook).Hash
$null = New-Item -ItemType Directory -Force $EvidenceDir
if (Get-ChildItem -LiteralPath $env:GITHUB_WORKSPACE -Recurse -File -Filter Cargo.toml | Where-Object { $_.FullName -like '*\crates\zed\Cargo.toml' }) {
    throw 'Smoke workspace contains a product source checkout'
}
if (Test-Path -LiteralPath (Join-Path $PackageDir '.zed')) { throw 'Unexpected project configuration in the sample package' }
Add-Type -AssemblyName System.Drawing,System.Windows.Forms,UIAutomationClient,UIAutomationTypes,WindowsBase
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Runtime.InteropServices;
public static class NotebookWindow {
    [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr w, out Rect r);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr w);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr w, int n);
    [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr w, int x, int y, int width, int height, bool paint);
    [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
    [DllImport("user32.dll")] public static extern void keybd_event(byte key, byte scan, uint flags, UIntPtr extra);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, int x, int y, int data, UIntPtr extra);
    public static bool HasChart(Bitmap frame) {
        int[,] bars = {{8,127,115}, {207,91,42}, {84,96,173}};
        for (int y = 50; y < frame.Height; y += 4) {
            int bar = 0, run = 0;
            for (int x = 0; x < frame.Width; x += 3) {
                Color p = frame.GetPixel(x, y);
                if (Math.Abs(p.R-bars[bar,0]) <= 4 && Math.Abs(p.G-bars[bar,1]) <= 4 && Math.Abs(p.B-bars[bar,2]) <= 4) {
                    if (++run >= 5) { if (++bar == 3) return true; run = 0; }
                } else { run = 0; }
            }
        }
        return false;
    }
}
'@

function Focus-Editor {
    for ($i = 0; $i -lt 10; $i++) {
        if ([NotebookWindow]::GetForegroundWindow() -eq $window) { return }
        [NotebookWindow]::keybd_event(0x12,0,0,[UIntPtr]::Zero)
        [NotebookWindow]::keybd_event(0x12,0,2,[UIntPtr]::Zero)
        [void][NotebookWindow]::SetForegroundWindow($window)
        Start-Sleep -Milliseconds 300
    }
    throw 'Zed did not acquire foreground focus'
}
function Window-Rect {
    $r = New-Object NotebookWindow+Rect
    if (![NotebookWindow]::GetWindowRect($window,[ref]$r)) { throw 'Could not read Zed window geometry' }
    return $r
}
function Capture($name) {
    Focus-Editor
    $r = Window-Rect
    $frame = New-Object Drawing.Bitmap (($r.Right-$r.Left), ($r.Bottom-$r.Top))
    $g = [Drawing.Graphics]::FromImage($frame)
    try {
        $g.CopyFromScreen($r.Left,$r.Top,0,0,$frame.Size)
        $frame.Save((Join-Path $EvidenceDir "$name.png"),[Drawing.Imaging.ImageFormat]::Png)
        return [NotebookWindow]::HasChart($frame)
    } finally { $g.Dispose(); $frame.Dispose() }
}
function Element($name) {
    $condition = New-Object Windows.Automation.PropertyCondition ([Windows.Automation.AutomationElement]::NameProperty,$name)
    $root = [Windows.Automation.AutomationElement]::FromHandle($window)
    return $root.FindFirst([Windows.Automation.TreeScope]::Descendants,$condition)
}
function Click-At($x,$y) {
    Focus-Editor
    [void][NotebookWindow]::SetCursorPos([int]$x,[int]$y)
    [NotebookWindow]::mouse_event(2,0,0,0,[UIntPtr]::Zero)
    [NotebookWindow]::mouse_event(4,0,0,0,[UIntPtr]::Zero)
}
function Click-Element($el) {
    $rect = $el.Current.BoundingRectangle
    if ($rect.IsEmpty -or $rect.Width -lt 1 -or $rect.Height -lt 1) { throw 'Empty control bounds' }
    Click-At ($rect.Left+$rect.Width/2) ($rect.Top+$rect.Height/2)
}
function Show-Chart($name) {
    for ($i = 0; $i -lt 6; $i++) {
        Start-Sleep -Milliseconds 750
        if (Capture $name) { return }
        $r = Window-Rect
        [void][NotebookWindow]::SetCursorPos(($r.Left+150),([int](($r.Top+$r.Bottom)/2)))
        [NotebookWindow]::mouse_event(2048,0,0,-240,[UIntPtr]::Zero)
    }
    throw "The three saved chart bars were not visible in $name.png"
}

$root = Join-Path $env:RUNNER_TEMP ("notebook-smoke-" + [Guid]::NewGuid().ToString('N'))
$userData = Join-Path $root 'profile'
$config = New-Item -ItemType Directory -Path (Join-Path $userData 'config') -Force
[IO.File]::WriteAllText((Join-Path $config.FullName 'settings.json'), '{"enable_language_server":false,"auto_update":false,"auto_install_extensions":{"html":false},"use_system_prompts":false,"telemetry":{"metrics":false,"diagnostics":false}}')
$env:JUPYTER_PATH = Join-Path $root 'jupyter'
$env:JUPYTER_DATA_DIR = $env:JUPYTER_PATH
foreach ($trial in @('Alpine','Coastal','Delta','Forest','Mesa','Valley')) {
    $specDir = (New-Item -ItemType Directory -Force (Join-Path $env:JUPYTER_PATH "kernels\$trial")).FullName
    $stub = (New-Item -ItemType File (Join-Path $specDir 'python.exe')).FullName
    $spec = @{ argv = @($stub,'-f','{connection_file}'); display_name = "$trial synthetic Python environment for the portable notebook layout demonstration"; language = 'python' }
    [IO.File]::WriteAllText((Join-Path $specDir 'kernel.json'),($spec | ConvertTo-Json))
}

$env:ZED_DISABLE_STAFF = '1'
$env:Path = "$env:SystemRoot\System32;$env:SystemRoot"
foreach ($name in @('LOCAL_NOTEBOOK_DEV','JUPYTER_SERVER','JUPYTER_TOKEN','PYTHONPATH','VIRTUAL_ENV','CONDA_PREFIX','CARGO_MANIFEST_DIR','CARGO_TARGET_DIR')) {
    [Environment]::SetEnvironmentVariable($name,$null,'Process')
}
$report = [ordered]@{ runner=$env:ImageOS; imageVersion=$env:ImageVersion; executable=$exe; sha256=(Get-FileHash $exe).Hash; notebookSha256=$before; launchPath=$env:Path; sourceCheckout=$false }
$trace = 'notebook-smoke-python'
Register-CimIndicationEvent -Query "SELECT * FROM Win32_ProcessStartTrace WHERE ProcessName = 'python.exe' OR ProcessName = 'pythonw.exe' OR ProcessName = 'python3.exe'" -SourceIdentifier $trace | Out-Null
try {
    $launchArgs = '--user-data-dir "{0}" "{1}" "{2}"' -f $userData,$PackageDir,$notebook
    $editor = Start-Process -FilePath $exe -WorkingDirectory $PackageDir -ArgumentList $launchArgs -PassThru
    for ($i = 0; $i -lt 120; $i++) {
        $editor.Refresh()
        if ($editor.HasExited) { throw "Zed exited before opening its window ($($editor.ExitCode))" }
        if ($editor.MainWindowHandle -ne 0) { break }
        Start-Sleep -Seconds 1
    }
    $window = $editor.MainWindowHandle
    if ($window -eq 0 -or $editor.Path -ne $exe) { throw 'Expected a window owned by the extracted Zed.exe' }
    [void][NotebookWindow]::SetThreadDpiAwarenessContext([IntPtr](-4))
    $desktop = [Windows.Forms.Screen]::PrimaryScreen.WorkingArea
    [void][NotebookWindow]::ShowWindow($window,9)
    if (![NotebookWindow]::MoveWindow($window,$desktop.Left,$desktop.Top,$desktop.Width,$desktop.Height,$true)) { throw 'Could not size the Zed window' }
    Focus-Editor
    Start-Sleep -Seconds 12
    $trust = Element 'Trust and Continue'
    if ($trust) {
        Click-Element $trust
    } elseif (!(Capture '01-saved-chart')) {
        # The hosted harness confirms the project trust modal with Enter; never enable global trust.
        [Windows.Forms.SendKeys]::SendWait('{ENTER}')
    }
    Start-Sleep -Seconds 2
    Show-Chart '01-saved-chart'
    $report.savedChartVisible = $true
    $narrowWidth = [Math]::Min(700,$desktop.Width)
    if (![NotebookWindow]::MoveWindow($window,$desktop.Left,$desktop.Top,$narrowWidth,$desktop.Height,$true)) { throw 'Could not resize to narrow window' }
    Show-Chart '02-narrow-controls'
    $r = Window-Rect
    $report.narrowWindowWidth = $r.Right-$r.Left
    $report.narrowChartVisible = $true

    $picker = Element 'Select Kernel'
    if ($picker) {
        Click-Element $picker
    } else {
        # On the standard 1024x768 image the kernel bar sits immediately above the global status bar.
        Click-At ($r.Left+52) ($r.Bottom-48)
    }
    Start-Sleep -Seconds 2
    [void](Capture '03-picker-attempt')
    $rootElement = [Windows.Automation.AutomationElement]::FromHandle($window)
    $labels = @($rootElement.FindAll([Windows.Automation.TreeScope]::Descendants,[Windows.Automation.Condition]::TrueCondition) | ForEach-Object { $_.Current.Name })
    $choices = @($labels | Where-Object { $_ -like '*synthetic Python environment*' } | Select-Object -Unique)
    if ($choices.Count -gt 0) {
        $report.pickerChoices = $choices
        Move-Item -Force (Join-Path $EvidenceDir '03-picker-attempt.png') (Join-Path $EvidenceDir '03-long-kernel-choices.png')
        $firstChoice = Element $choices[0]
        Click-Element $firstChoice
        Start-Sleep -Seconds 1
        [void](Capture '04-selected-kernel-status')
        $name = ($choices[0] -split ' \(',2)[0]
        $report.selectedLongKernel = [bool](Element $name)
    } else {
        $report.picker = 'No synthetic choice confirmed via UI Automation; inspect 03-picker-attempt.png'
    }

    $modules = @((Get-Process -Id $editor.Id).Modules | ForEach-Object { $_.FileName } | Sort-Object -Unique)
    $modules | Set-Content (Join-Path $EvidenceDir 'module-paths.txt')
    $foreign = @($modules | Where-Object { !$_.StartsWith("$PackageDir\",[StringComparison]::OrdinalIgnoreCase) -and !$_.StartsWith("$env:SystemRoot\",[StringComparison]::OrdinalIgnoreCase) })
    if ($modules -notcontains $exe -or $foreign.Count) { throw "Unexpected executable/module origin: $($foreign -join ', ')" }
    $report.onlyPackageOrWindowsModules = $true
    if ((Get-FileHash $notebook).Hash -ne $before) { throw 'The passive UI smoke modified Notebook.ipynb' }
    $report.sampleUnchanged = $true
    $started = @(Get-Event -SourceIdentifier $trace -ErrorAction SilentlyContinue)
    if ($started.Count) { throw 'A Python process started during passive viewing or kernel selection' }
    $report.pythonStarted = $false
} finally {
    if ($editor -and !$editor.HasExited) { Stop-Process -Id $editor.Id -Force }
    Unregister-Event -SourceIdentifier $trace
    $report | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $EvidenceDir 'smoke.json')
}
Write-Host 'Verified saved chart in the extracted editor, narrow layout capture, unchanged sample, and runtime module paths.'
