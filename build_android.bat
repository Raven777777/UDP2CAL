@echo off
setlocal enabledelayedexpansion

echo.
echo ========================================
echo    UDP2Mic Android Build Script
echo ========================================
echo.
echo Select target ABI:
echo   [1] arm64-v8a   (64-bit)
echo   [2] armeabi-v7a (32-bit)
echo.

CHOICE /C 12 /N /M "[1/2]: "
if errorlevel 2 goto v7a
goto v8a

:v8a
set ABI=arm64-v8a
goto build

:v7a
set ABI=armeabi-v7a
goto build

:build
echo.
echo ========================================
echo    ABI: %ABI%
echo ========================================
echo.

cd /d "%~dp0android"

echo [1/3] Prerequisites check...
echo    SDK: %ANDROID_HOME%
echo    JAVA: %JAVA_HOME%
echo.

echo [2/3] Building Release APK (%ABI%)...
call gradlew assembleRelease -PtargetAbi=%ABI% 2>&1
if errorlevel 1 (
    echo.
    echo [ERROR] Build failed.
    pause
    exit /b 1
)

echo.
if exist "app\build\outputs\apk\release\app-release.apk" (
    for %%A in ("app\build\outputs\apk\release\app-release.apk") do set size=%%~zA
    set /a sizekb=!size!/1024
    echo    APK built: !sizekb! KB

    echo.
    echo [3/3] Moving APK to project root...
    move /Y "app\build\outputs\apk\release\app-release.apk" "%~dp0udp2mic_%ABI%.apk" >nul
    echo    -> udp2mic_%ABI%.apk

    rmdir /s /q "app\build\outputs\apk\release" 2>nul

    echo.
    echo ========================================
    echo    BUILD SUCCESS  %ABI%
    echo ========================================
) else (
    echo.
    echo [ERROR] APK not found.
)

pause