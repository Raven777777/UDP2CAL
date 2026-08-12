@echo off
setlocal

cd /d "%~dp0"
echo Building Windows Window Demo (debug)...
cargo build --manifest-path "%~dp0Cargo.toml"

if errorlevel 1 (
    echo.
    echo Debug build failed.
    exit /b 1
)

echo.
echo Debug build succeeded:
echo %~dp0target\debug\windows-window-demo.exe
exit /b 0
