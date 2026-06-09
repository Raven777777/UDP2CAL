@echo off
setlocal enabledelayedexpansion
title UDP2CAL Windows Build

echo ========================================
echo    UDP2CAL Windows Build Script
echo ========================================
echo.

:::: ---- Build Mode Selection ----
echo Select build mode:
echo   [1] Release  (optimized, ~5.98 MB)
echo   [2] Debug    (unoptimized, with debug info)
echo.
CHOICE /C 12 /N /M "[1/2]: "
if errorlevel 2 (
    set BUILD_MODE=debug
    set BUILD_TARGET=debug
    set BUILD_TAG=-debug
) else (
    set BUILD_MODE=release
    set BUILD_TARGET=release
    set BUILD_TAG=
)
echo.

:::: ---- Target Architecture Selection ----
echo Select target architecture:
echo   [1] x86_64  (64-bit)
echo   [2] i686    (32-bit)
echo.
CHOICE /C 12 /N /M "[1/2]: "
if errorlevel 2 (
    set TARGET=i686-pc-windows-msvc
    set ARCH=_x86
) else (
    set TARGET=x86_64-pc-windows-msvc
    set ARCH=_x64
)

:select_upx
:::: ---- UPX Compression Option ----
set USE_UPX=
set UPX_PATH=%~dp0upx-5.1.1-win64\upx.exe
if exist "%UPX_PATH%" (
    echo.
    echo UPX found. Compress executable?
    echo   [1] Yes
    echo   [2] No
    echo.
    CHOICE /C 12 /N /M "[1/2]: "
    if not errorlevel 2 set USE_UPX=1
)

:::: ---- Build ----
echo.
echo ========================================
echo    Mode:   %BUILD_MODE%
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

:::: Ensure target is installed
echo.
echo [1.5/3] Ensuring target %TARGET%...
rustup target add %TARGET% >nul 2>&1
if errorlevel 1 (
    echo [ERROR] Failed to add target %TARGET%
    pause
    exit /b 1
)
echo   Target: OK

:::: Clean stale artifacts from --target-less builds
if exist "target\%BUILD_TARGET%\udp2cal.exe" (
    echo   Clean: removed old target\%BUILD_TARGET%\udp2cal.exe
    del "target\%BUILD_TARGET%\udp2cal.exe" 2>nul
)

echo.
echo [2/3] Building %BUILD_MODE% (%TARGET%)...
set CMAKE_POLICY_VERSION_MINIMUM=3.5
set CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_STRIP=none
if "%BUILD_MODE%"=="debug" (
    cargo build --target %TARGET%
) else (
    cargo build --release --target %TARGET%
)
if errorlevel 1 (
    echo.
    echo [ERROR] Build failed
    pause
    exit /b 1
)

set SRC="%cd%\target\%TARGET%\%BUILD_TARGET%\udp2cal.exe"

echo.
echo [3/3] Copying to project root...
if exist %SRC% (
    for %%A in (%SRC%) do set size=%%~zA
    set /a sizekb=!size!/1024
    echo   udp2cal%ARCH%%BUILD_TAG%.exe (!sizekb! KB^)

    :: UPX compression (release only)
    set UPX_TAG=
    if "%BUILD_MODE%"=="release" if defined USE_UPX (
        echo.
        echo   Compressing with UPX...
        "%UPX_PATH%" --best %SRC% >nul
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
    set OUT="%~dp0udp2cal%ARCH%%BUILD_TAG%%UPX_TAG%.exe"
    copy /Y %SRC% !OUT! >nul
    for %%A in (!OUT!) do set finalsize=%%~zA
    set /a finalsizekb=!finalsize!/1024
    echo.
    echo ========================================
    echo    BUILD SUCCESS
    echo    mode:   %BUILD_MODE%
    echo    target: %TARGET%
    echo    output: udp2cal%ARCH%%BUILD_TAG%%UPX_TAG%.exe
    echo    size:   !finalsizekb! KB
    echo ========================================
) else (
    echo [ERROR] Build artifact not found at:
    echo   %SRC%
)

pause