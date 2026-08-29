@echo off
net session >nul 2>&1
if %errorLevel% neq 0 (
    if "%~1"=="" (
        powershell -Command "Start-Process -FilePath '%~f0' -Verb RunAs"
    ) else (
        powershell -Command "Start-Process -FilePath '%~f0' -ArgumentList '%*' -Verb RunAs"
    )
    exit /b
)

cd /d "%~dp0"
start "" "dependencies\python\pythonw.exe" "main.pyw" %*