@echo off

SET CURRENT_DIR=%cd%

cargo build --release --manifest-path "%~dp0Cargo.toml" || goto :error

"%~dp0target\release\fake-onvif-cam.exe" --config "%~dp0cameras.toml" || goto :error

:ok
cd "%CURRENT_DIR%"
exit /b 0

:error
cd "%CURRENT_DIR%"
exit /b %errorlevel%
