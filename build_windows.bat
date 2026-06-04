@echo off
setlocal enabledelayedexpansion
title UDP2Mic Windows Build
echo ========================================
echo   UDP2Mic Windows Build
echo ========================================
echo.

cd /d "%~dp0windows"
if errorlevel 1 (
    echo [ERROR] Cannot enter windows directory
    pause
    exit /b 1
)

echo [1/3] Checking Rust toolchain...
rustc --version >nul 2>&1
if errorlevel 1 (
    echo [ERROR] Rust not found: https://rustup.rs
    pause
    exit /b 1
)
echo   Rust: OK

echo.
echo [2/3] Building Release...
set CMAKE_POLICY_VERSION_MINIMUM=3.5
cargo build --release
if errorlevel 1 (
    echo [ERROR] Build failed
    pause
    exit /b 1
)

echo.
echo [3/3] Copying to project root...
if exist "target\release\udp2mic.exe" (
    copy /Y "target\release\udp2mic.exe" "%~dp0udp2mic.exe" >nul
    for %%A in ("%~dp0udp2mic.exe") do set size=%%~zA
    set /a sizekb=!size!/1024
    echo   udp2mic.exe (!sizekb! KB^)
    echo.
    echo ========================================
    echo   BUILD SUCCESS
    echo   root: %~dp0udp2mic.exe
    echo ========================================
) else (
    echo [ERROR] Output file not found
)

pause
