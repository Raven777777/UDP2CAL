@echo off
setlocal enabledelayedexpansion

echo.
echo ========================================
echo    UDP2CAL Android Build Script
echo ========================================
echo.
echo Select project and ABI:
echo   [1] Modern - arm64-v8a   (64-bit)
echo   [2] Modern - armeabi-v7a (32-bit)
echo   [3] Old - arm64-v8a   (64-bit, Android 6~8)
echo   [4] Old - armeabi-v7a (32-bit, Android 6~8)
echo.

choice /c 1234 /n /m "Pick [1/2/3/4]: "

if errorlevel 4 goto old_v7a
if errorlevel 3 goto old_v8a
if errorlevel 2 goto v7a
goto v8a

:v8a
set ABI=arm64-v8a
set PROJ=android
set PREFIX=udp2cal
goto build

:v7a
set ABI=armeabi-v7a
set PROJ=android
set PREFIX=udp2cal
goto build

:old_v8a
set ABI=arm64-v8a
set PROJ=android_old
set PREFIX=udp2cal_old
goto build

:old_v7a
set ABI=armeabi-v7a
set PROJ=android_old
set PREFIX=udp2cal_old
goto build

:build
echo.
echo ========================================
echo    Project: %PROJ%
echo    ABI:     %ABI%
echo ========================================
echo.

cd /d "%~dp0%PROJ%"

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
set APK_SRC=app\build\outputs\apk\release\app-release.apk
if exist "%APK_SRC%" (
    for %%A in ("%APK_SRC%") do set size=%%~zA
    set /a sizekb=!size!/1024
    echo    APK built: !sizekb! KB

    echo.
    echo [3/3] Moving APK to project root...
    move /Y "%APK_SRC%" "%~dp0%PREFIX%_%ABI%.apk" >nul
    echo    -^> %PREFIX%_%ABI%.apk

    rmdir /s /q "app\build\outputs\apk\release" 2>nul

    echo.
    echo ========================================
    echo    BUILD SUCCESS  %PREFIX%_%ABI%.apk
    echo ========================================
) else (
    echo.
    echo [ERROR] APK not found.
)

pause
