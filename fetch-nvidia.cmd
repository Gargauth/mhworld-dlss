@echo off
rem Runs fetch-nvidia.ps1, which sits beside this file, and keeps the window
rem open so that what it printed can be read. Double-click this file.
rem
rem -ExecutionPolicy Bypass applies to this one run only. Windows refuses to
rem run a downloaded script otherwise, and nothing on the system is changed
rem by passing it.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0fetch-nvidia.ps1" %*
echo.
pause
