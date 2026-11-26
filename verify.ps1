$ErrorActionPreference = 'Stop'
if ($env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_ARCH -ne 'X64') { throw 'Hosted Windows required' }
Add-Type -AssemblyName System.Windows.Forms,System.Drawing,UIAutomationClient,UIAutomationTypes
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class Win {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr w);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr w, int cmd);
    [DllImport("user32.dll")] public static extern void keybd_event(byte key,byte scan,uint flags,UIntPtr extra);
}
'@
$proof = (mkdir "$env:RUNNER_TEMP\proof").FullName
$project = (mkdir "$env:RUNNER_TEMP\first-run").FullName
$profileDir = (mkdir "$env:RUNNER_TEMP\first-profile").FullName
$notebook = Join-Path $project 'Arithmetic.ipynb'
$exe = "$env:RUNNER_TEMP\preview\Zed.exe"
$install = "$profileDir\notebook-kernels"
function Note($s) { Write-Host $s; Add-Content "$proof\evidence.txt" $s }
function Shot($name) {
    $rect = [Windows.Forms.Screen]::PrimaryScreen.Bounds
    $bmp = New-Object Drawing.Bitmap($rect.Width,$rect.Height)
    $g = [Drawing.Graphics]::FromImage($bmp)
    try { $g.CopyFromScreen($rect.Left,$rect.Top,0,0,$bmp.Size); $bmp.Save("$proof\$name.png",[Drawing.Imaging.ImageFormat]::Png) } finally { $g.Dispose(); $bmp.Dispose() }
}
function Keys($key,$seconds=1) {
    for ($i=0; $i -lt 8 -and [Win]::GetForegroundWindow() -ne $win; $i++) {
        [Win]::keybd_event(0x12,0,0,[UIntPtr]::Zero); [Win]::keybd_event(0x12,0,2,[UIntPtr]::Zero)
        [void][Win]::SetForegroundWindow($win); Start-Sleep -Milliseconds 350
    }
    if ([Win]::GetForegroundWindow() -ne $win) { throw 'Editor is not foreground' }
    [Windows.Forms.SendKeys]::SendWait($key)
    Start-Sleep -Seconds $seconds
}
function Launch {
    $script:editor = Start-Process $exe -WorkingDirectory $project -ArgumentList ('--user-data-dir "{0}" "{1}"' -f $profileDir,$notebook) -PassThru
    for ($i=0; $i -lt 60; $i++) { $editor.Refresh(); if ($editor.HasExited) { throw 'Editor exited at startup' }; if ($editor.MainWindowHandle) { break }; Start-Sleep -Seconds 1 }
    $script:win = $editor.MainWindowHandle
    if (!$win -or $editor.Path -ne $exe) { throw 'Wrong editor process' }
    [void][Win]::ShowWindow($win,3); Start-Sleep -Seconds 10
}
function Quit {
    Keys '^q' 1
    if (!$editor.WaitForExit(30000)) { throw 'Quit timed out' }
}
function Choose-Setup($choice,$shot) {
    $root = [Windows.Automation.AutomationElement]::RootElement
    $processFilter = New-Object Windows.Automation.PropertyCondition ([Windows.Automation.AutomationElement]::ProcessIdProperty,$editor.Id)
    $label = New-Object Windows.Automation.PropertyCondition ([Windows.Automation.AutomationElement]::NameProperty,'Set up Python for notebooks?')
    $button = New-Object Windows.Automation.PropertyCondition ([Windows.Automation.AutomationElement]::NameProperty,$choice)
    for ($i=0;$i -lt 45;$i++) {
        foreach ($w in $root.FindAll([Windows.Automation.TreeScope]::Children,$processFilter)) {
            if ($w.FindFirst([Windows.Automation.TreeScope]::Descendants,$label)) {
                $b=$w.FindFirst([Windows.Automation.TreeScope]::Descendants,$button)
                if (!$b) { throw "Consent button missing: $choice" }
                Shot $shot
                $b.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
                Note "Identified default Windows setup prompt and invoked '$choice'"; Start-Sleep -Seconds 2; return
            }
        }
        Start-Sleep -Milliseconds 500
    }
    throw 'Expected setup consent prompt was not found'
}
function PythonEvents { @(Get-Event -SourceIdentifier first-run-python -ErrorAction SilentlyContinue).Count }
function No-Setup($phase) {
    if ((Test-Path $install) -or (PythonEvents) -or (Get-FileHash $notebook).Hash -ne $initial) { throw "Unexpected mutation or Python start during $phase" }
    Note "$phase : zero Python starts, no notebook-kernels directory, notebook hash unchanged"
}
function Wait-Saved($oldPid) {
    for ($i=0; $i -lt 40; $i++) {
        Keys '^s' 1
        $n = Get-Content -Raw $notebook | ConvertFrom-Json
        $firstText = $n.cells[0].outputs[0].text -join ''
        $secondText = $n.cells[1].outputs[0].text -join ''
        if ($firstText -match 'process=(\d+); basic=7' -and $secondText -eq "dependent=42`n") {
            $processId = [int]$Matches[1]
            if ($processId -ne $oldPid -and $n.cells[0].execution_count -eq 1 -and $n.cells[1].execution_count -eq 2 -and $null -ne $n.cells[2].execution_count) {
                $p = Get-Process -Id $processId
                if ($p.Path -ne $privatePython) { throw "Result did not come from private interpreter" }
                $png = @($n.cells[2].outputs | Where-Object { $_.data.'image/png' })
                $err = @($n.cells[2].outputs | Where-Object { $_.output_type -eq 'error' })
                if ($png.Count -eq 1) {
                    $bytes = [Convert]::FromBase64String(($png[0].data.'image/png' -join ''))
                    $stream = New-Object IO.MemoryStream(,$bytes)
                    try { $image = [Drawing.Image]::FromStream($stream); Note "Delivered example code: rendered $($image.Width)x$($image.Height) PNG"; if ($image.Width -ne 280 -or $image.Height -ne 116) { throw 'Wrong chart dimensions' }; $image.Dispose() } finally { $stream.Dispose() }
                } elseif ($err.Count -eq 1 -and $err[0].ename -eq 'ModuleNotFoundError' -and $err[0].evalue -like '*PIL*') { Note 'Basic setup succeeded, packaged example requires additional Pillow' }
                else { throw 'Packaged sample produced an unexpected result' }
                Note "Run All -> Save: private Python PID $processId, counts 1,2; stdout basic=7 and dependent=42"; return $processId
            }
        }
    }
    throw 'Saved file did not acquire the expected fresh kernel results'
}
$sample = Get-Content -Raw "$env:RUNNER_TEMP\preview\Notebook.ipynb" | ConvertFrom-Json
$codes = @("import os`nx = 7`nprint(f'process={os.getpid()}; basic={x}')","print(f'dependent={x * 6}')",($sample.cells[-1].source -join ''))
$cells = @(for ($i=0;$i -lt $codes.Length;$i++) { @{cell_type='code'; id="case-$i"; source=$codes[$i]; outputs=@(); execution_count=$null; metadata=@{}} })
$n=@{ nbformat=4; nbformat_minor=5; metadata=@{language_info=@{name='python'}}; cells=$cells }
[IO.File]::WriteAllText($notebook,($n|ConvertTo-Json -Depth 10))
$initial = (Get-FileHash $notebook).Hash
$env:Path="$env:SystemRoot\System32;$env:SystemRoot"
$env:JUPYTER_DATA_DIR=(mkdir "$env:RUNNER_TEMP\empty-jupyter").FullName; $env:JUPYTER_PATH=$env:JUPYTER_DATA_DIR
foreach ($name in 'PYTHONPATH','VIRTUAL_ENV','CONDA_PREFIX','LOCAL_NOTEBOOK_DEV','JUPYTER_SERVER','JUPYTER_TOKEN') { [Environment]::SetEnvironmentVariable($name,$null,'Process') }
Register-CimIndicationEvent -Query "SELECT * FROM Win32_ProcessStartTrace WHERE ProcessName = 'python.exe' OR ProcessName = 'python3.exe' OR ProcessName = 'pythonw.exe'" -SourceIdentifier first-run-python | Out-Null
try {
    Launch
    Shot '01-open'
    Keys '^+%s' 2
    No-Setup 'Restricted open'
    Keys '^+{ENTER}' 2
    Shot '02-restricted-run'
    No-Setup 'Restricted Run All'
    Keys '^+%s' 1
    Keys '{ENTER}' 3
    Keys '^+{ENTER}' 1
    Choose-Setup 'Cancel' '03-offer'
    Shot '04-cancel'
    Keys '^s' 1
    No-Setup 'Canceled trusted Run All'
    Keys '^+{ENTER}' 1
    Choose-Setup 'Set Up Python' '05-consent'
    for ($i=0;$i -lt 240;$i++) {
        $marker = Get-ChildItem $install -Recurse -File -Filter complete -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($marker) { break }
        Start-Sleep -Seconds 2
    }
    if (!$marker) { throw 'Private setup did not write completion marker within 8 minutes' }
    $privatePython = Join-Path $marker.DirectoryName 'python\python.exe'
    $runtimeHash = (Get-FileHash $privatePython).Hash
    $markerTime=$marker.LastWriteTimeUtc.Ticks
    $firstPid = Wait-Saved -1
    Copy-Item $notebook "$proof\saved.ipynb"
    Shot '06-executed'
    Keys '^+r' 2
    Quit
    $firstHash = (Get-FileHash $notebook).Hash
    $count = PythonEvents
    Launch
    Shot '07-reopened'
    if ((PythonEvents) -ne $count -or (Get-FileHash $notebook).Hash -ne $firstHash) { throw 'Reopen launched Python or changed the file' }
    Note 'Reopened in new editor process without starting Python or changing saved file'
    Keys '^+{ENTER}' 1
    $secondPid = Wait-Saved $firstPid
    if ((Get-Item $marker.FullName).LastWriteTimeUtc.Ticks -ne $markerTime -or (Get-FileHash $privatePython).Hash -ne $runtimeHash) { throw 'Private runtime replaced during reuse' }
    Note 'Second Run All used a new private kernel PID and the preexisting runtime completion marker; no second consent'
    Copy-Item $notebook "$proof\rerun.ipynb"
    Shot '08-rerun'
    Keys '^+r' 2
    Quit
} catch {
    Shot 'failure'
    throw
} finally {
    Unregister-Event -SourceIdentifier first-run-python
    if ($editor -and !$editor.HasExited) { Stop-Process -Id $editor.Id -Force }
}
