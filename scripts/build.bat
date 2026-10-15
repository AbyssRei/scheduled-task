@echo off
rem Build entry for scheduled-task (Rust): launches build.ps1 via PowerShell 7 (pwsh).
rem NOTE: keep this file ASCII-only -- cmd parses bat files in the OEM codepage,
rem       non-ASCII bytes will corrupt the parser. Chinese output lives in build.ps1.

set "SCRIPT_DIR=%~dp0"

where pwsh >nul 2>nul
if errorlevel 1 (
    echo [ERROR] PowerShell 7 ^(pwsh^) not found in PATH.
    echo         Install pwsh first: https://aka.ms/powershell
    echo         Then re-run this script, or call: pwsh -File "%SCRIPT_DIR%build.ps1"
    exit /b 1
)

pwsh -NoProfile -File "%SCRIPT_DIR%build.ps1"
exit /b %errorlevel%
