@echo off

SET CURRENT_DIR=%cd%

"%~dp0target\release\fake-onvif-cam.exe" --config "%~dp0cameras.toml" || goto :error

:ok
cd "%CURRENT_DIR%"
exit /b 0

:error
cd "%CURRENT_DIR%"
exit /b %errorlevel%
