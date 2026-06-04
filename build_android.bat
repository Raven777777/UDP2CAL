@echo off
chcp 65001 >nul
setlocal enabledelayedexpansion

echo ========================================
echo    UDP2Mic Android 构建脚本
echo ========================================
echo.

cd /d "%~dp0android"

echo [1/3] 前置条件检查...
echo    需要: Android Studio + NDK 27+ + CMake 3.22+
echo    需要: JDK 17
echo.
echo    Android SDK 路径: %ANDROID_HOME%
echo    JAVA_HOME: %JAVA_HOME%
echo.

echo [2/3] 编译 Release APK (含自动签名)...
call gradlew assembleRelease 2>&1
if errorlevel 1 (
    echo [错误] 编译失败，请检查错误信息
    pause
    exit /b 1
)

echo.
if exist "app\build\outputs\apk\release\app-release.apk" (
    for %%A in ("app\build\outputs\apk\release\app-release.apk") do set size=%%~zA
    set /a sizekb=!size!/1024
    echo    APK: app\build\outputs\apk\release\app-release.apk (!sizekb! KB)

    echo.
    echo [3/3] 复制到根目录...
    copy /Y "app\build\outputs\apk\release\app-release.apk" "%~dp0udp2mic.apk" >nul
    
    if exist "%~dp0udp2mic-release.apk" (
        for %%A in ("%~dp0udp2mic.apk") do set rootsize=%%~zA
        set /a rootkb=!rootsize!/1024
        echo    根目录: udp2mic.apk (!rootkb! KB)
    )

    echo.
    echo ========================================
    echo    构建成功！
    echo    APK: app\build\outputs\apk\release\app-release.apk (已签名)
    echo    根目录: %~dp0udp2mic.apk
    echo ========================================

pause