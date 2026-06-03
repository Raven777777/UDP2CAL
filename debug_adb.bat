@echo off
chcp 65001 >nul
title UDP2Mic ADB 调试工具
set ADB=C:\Android\sdk\platform-tools\adb.exe

echo ========================================
echo   UDP2Mic ADB 自动调试工具
echo ========================================
echo.

:: 检查设备
"%ADB%" devices 2>nul | find "device" >nul
if errorlevel 1 (
    echo [错误] 未检测到 Android 设备
    echo 请确认: 1. USB 调试已开启 2. adb 驱动已安装
    pause
    exit /b 1
)

:: 解析参数
set BUILD=0
set INSTALL=0
set CLEAR=0
set MONITOR=0
set WINLOG=0

:parse
if "%1"=="" goto :menu
if /i "%1"=="--build" set BUILD=1&shift&goto :parse
if /i "%1"=="--install" set INSTALL=1&shift&goto :parse
if /i "%1"=="--clear" set CLEAR=1&shift&goto :parse
if /i "%1"=="--monitor" set MONITOR=1&shift&goto :parse
if /i "%1"=="--winlog" set WINLOG=1&shift&goto :parse
if /i "%1"=="--all" set BUILD=1&set INSTALL=1&set CLEAR=1&set MONITOR=1&shift&goto :parse
shift
goto :parse

:menu
if %BUILD%==0 if %INSTALL%==0 if %CLEAR%==0 if %MONITOR%==0 if %WINLOG%==0 (
    echo 用法: debug_adb.bat [选项]
    echo.
    echo 选项:
    echo   --build     编译 Debug APK
    echo   --install   安装 APK 到设备
    echo   --clear     清空 Logcat 缓冲
    echo   --monitor   启动 Logcat 监控 (过滤 OpusEncoder/CaptureService/MainActivity)
    echo   --winlog    同时打开 Windows 调试日志 (udp2mic_debug.log)
    echo   --all       执行全部上述操作
    echo.
    echo 示例:
    echo   debug_adb.bat --all           一键编译+安装+监控
    echo   debug_adb.bat --monitor       仅启动 Logcat 监控
    echo.
    echo 快捷键:
    echo   Ctrl+C 退出监控
    echo.
    pause
    exit /b 0
)

setlocal enabledelayedexpansion

cd /d "%~dp0android"

if %BUILD%==1 (
    echo [1/5] 编译 Debug APK...
    call gradlew assembleDebug 2>&1
    if errorlevel 1 (
        echo [错误] 编译失败
        pause
        exit /b 1
    )
    echo   ✓ 编译成功
)

if %INSTALL%==1 (
    echo [2/5] 安装 APK 到设备...
    "%ADB%" install -r "app\build\outputs\apk\debug\app-debug.apk" 2>&1
    if errorlevel 1 (
        echo [信息] 签名冲突，尝试卸载后重装...
        "%ADB%" uninstall com.udp2mic.app >nul 2>&1
        "%ADB%" install "app\build\outputs\apk\debug\app-debug.apk" 2>&1
        if errorlevel 1 (
            echo [错误] 安装失败
            pause
            exit /b 1
        )
    )
    echo   ✓ 安装成功
)

if %CLEAR%==1 (
    echo [3/5] 清空 Logcat 缓冲...
    "%ADB%" logcat -c
    echo   ✓ 缓冲已清空
)

if %MONITOR%==1 (
    echo [4/5] 启动 Logcat 监控...
    echo.
    echo ========================================
    echo   请在手机上打开 UDP2Mic APP
    echo   开启"调试模式"开关
    echo   点击"开始采集"
    echo.
    echo   按 Ctrl+C 退出监控
    echo ========================================
    echo.
    "%ADB%" logcat -v time -s OpusEncoder:V CaptureService:V MainActivity:V
)

if %WINLOG%==1 (
    echo [5/5] 打开 Windows 调试日志...
    if exist "%~dp0windows\udp2mic_debug.log" (
        notepad "%~dp0windows\udp2mic_debug.log"
    ) else (
        echo   Windows 调试日志尚未生成（需在 Windows 端开启调试模式）
    )
)

endlocal
pause