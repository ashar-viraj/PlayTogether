@echo off
rem PlayTogether installer for Windows: double-click me. (On Linux, run install.sh.)
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1"
echo.
pause
