@echo off
SET LOGFILE=%~dp0run-8.log

:loop
"%~dp0target\release\fake-onvif-cam.exe" --config "%~dp0cameras-8.toml"
SET EXIT_CODE=%errorlevel%

IF %EXIT_CODE% EQU 0 goto :done
IF %EXIT_CODE% EQU -1073741510 goto :interrupted

echo [%DATE% %TIME%] Process exited with code %EXIT_CODE%, restarting... >> "%LOGFILE%"
goto :loop

:interrupted
echo [%DATE% %TIME%] Stopped by user (Ctrl+C). >> "%LOGFILE%"
exit /b 0

:done
echo [%DATE% %TIME%] Process exited cleanly (code 0). >> "%LOGFILE%"
exit /b 0
