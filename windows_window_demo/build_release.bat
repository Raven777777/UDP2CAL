@echo off
setlocal

cd /d "%~dp0"
echo Building Windows Window Demo (release)...
cargo build --release --manifest-path "%~dp0Cargo.toml"

if errorlevel 1 (
    echo.
    echo Release build failed.
    exit /b 1
)

echo.
echo Release build succeeded:
echo %~dp0target\release\windows-window-demo.exe
exit /b 0
