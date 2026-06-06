@echo off
setlocal enabledelayedexpansion
title UDP2Mic Windows Build

echo ========================================
echo    UDP2Mic Windows Build Script
echo ========================================
echo.

:: ---- Target Architecture Selection ----
echo Select target architecture:
echo   [1] x86_64  (64-bit)
echo   [2] i686    (32-bit)
echo.
CHOICE /C 12 /N /M "[1/2]: "
if errorlevel 2 set TARGET=i686-pc-windows-msvc&set ARCH=_x86&goto select_upx
set TARGET=x86_64-pc-windows-msvc&set ARCH=_x64

:: ---- UPX Compression Option ----
:select_upx
set USE_UPX=
where upx >nul 2>&1
if not errorlevel 1 (
    echo.
    echo UPX found. Compress executable?
    echo   [1] Yes
    echo   [2] No
    echo.
    CHOICE /C 12 /N /M "[1/2]: "
    if not errorlevel 2 set USE_UPX=1
)

:: ---- Build ----
echo.
echo ========================================
echo    Target: %TARGET%  (%ARCH%)
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

:: Ensure target is installed
echo.
echo [1.5/3] Ensuring target %TARGET%...
rustup target add %TARGET% >nul 2>&1
if errorlevel 1 (
    echo [ERROR] Failed to add target %TARGET%
    pause
    exit /b 1
)
echo   Target: OK

:: Clean stale artifacts from previous --target-less builds
if exist "target\release\udp2mic.exe" (
    echo   Clean: removed old target\release\udp2mic.exe
    del "target\release\udp2mic.exe" 2>nul
)

echo.
echo [2/3] Building Release (%TARGET%)...
set CMAKE_POLICY_VERSION_MINIMUM=3.5
cargo build --release --target %TARGET%
if errorlevel 1 (
    echo.
    echo [ERROR] Build failed
    pause
    exit /b 1
)

set SRC="%cd%\target\%TARGET%\release\udp2mic.exe"

echo.
echo [3/3] Copying to project root...
if exist %SRC% (
    for %%A in (%SRC%) do set size=%%~zA
    set /a sizekb=!size!/1024
    echo   udp2mic%ARCH%.exe (!sizekb! KB^)

    :: UPX compression
    set UPX_TAG=
    if defined USE_UPX (
        echo.
        echo   Compressing with UPX...
        upx --best %SRC% >nul
        if not errorlevel 1 (
            for %%A in (%SRC%) do set size2=%%~zA
            set /a sizekb2=!size2!/1024
            echo   UPX: !sizekb! KB -^> !sizekb2! KB
            set UPX_TAG=.upx
        ) else (
            echo   UPX: skipped
        )
    )

    :: Copy final binary to project root
    set OUT="%~dp0udp2mic%ARCH%!UPX_TAG!.exe"
    copy /Y %SRC% !OUT! >nul
    for %%A in (!OUT!) do set finalsize=%%~zA
    set /a finalsizekb=!finalsize!/1024
    echo.
    echo ========================================
    echo    BUILD SUCCESS
    echo    target: %TARGET%
    echo    output: udp2mic%ARCH%!UPX_TAG!.exe
    echo    size:   !finalsizekb! KB
    echo ========================================
) else (
    echo [ERROR] Build artifact not found at:
    echo   %SRC%
)

pause