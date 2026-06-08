@echo off
chcp 65001 >nul
setlocal enabledelayedexpansion

echo ========================================
echo    UDP2CAL APK 安装脚本
echo ========================================
echo.

cd /d "%~dp0"

echo [1/3] 卸载旧版本...
C:\Android\sdk\platform-tools\adb.exe uninstall com.udp2cal.app 2>nul
if errorlevel 1 (
    echo    未安装旧版本或卸载失败，继续安装...
) else (
    echo    旧版本已卸载
)
echo.

echo [2/3] 检查 APK 文件...
if not exist "udp2cal_old_armeabi-v7a.apk" (
    echo [错误] 未找到 udp2cal.apk，请先运行 build_android.bat
    pause
    exit /b 1
)
for %%A in ("udp2cal_old_armeabi-v7a.apk") do set size=%%~zA
set /a sizekb=!size!/1024
echo    APK: udp2cal_old_armeabi-v7a.apk (!sizekb! KB)
echo.

echo [3/3] 安装新版本...
C:\Android\sdk\platform-tools\adb.exe install -r udp2cal_old_armeabi-v7a.apk
if errorlevel 1 (
    echo [错误] 安装失败，请检查设备连接
    pause
    exit /b 1
)

echo.
echo ========================================
echo    安装成功！
echo ========================================
pause
