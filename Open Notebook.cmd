@echo off
setlocal DisableDelayedExpansion
set "ZED_NOTEBOOK_BUNDLE=%~dp0"
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -NonInteractive -Command "$profileDir = Join-Path $env:TEMP ('zed-notebook-' + [guid]::NewGuid()); & (Join-Path $env:ZED_NOTEBOOK_BUNDLE 'Zed.exe') --user-data-dir $profileDir (Join-Path $env:ZED_NOTEBOOK_BUNDLE 'Notebook.ipynb')"
