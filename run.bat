@echo off

SET CURRENT_DIR=%cd%

cd %~dp0target\release
fake-onvif-cam.exe --config ..\..\cameras.toml || goto :error

:ok
cd "%CURRENT_DIR%"
exit /b 0

:error
cd "%CURRENT_DIR%"
exit /b %errorlevel%
