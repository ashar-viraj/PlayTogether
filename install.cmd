:; # PlayTogether installer for Windows AND Linux: one file, it works out which one it's on.
:; #   Windows: double-click install.cmd        Linux: sh install.cmd
:; # (Windows reads the ":;" lines as labels and skips them; Linux shells run them and stop here.)
:; exec bash "$(cd "$(dirname "$0")" && pwd)/install.sh" "$@"
@echo off
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1"
echo.
pause
