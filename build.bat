@echo off
rem ============================================================================
rem  UDP2CAL 统一构建入口
rem
rem  用法:
rem    build.bat                                  交互式菜单
rem    build.bat android --modern --abi arm64-v8a --release
rem    build.bat win --x64 --release --upx
rem    build.bat adb --all
rem    build.bat doctor --fix
rem
rem  本文件只做一件事：定位可用的 Python 3.8+ 并转发到 scripts\build.py。
rem  全部构建/检测逻辑都在 scripts 目录内，便于维护与复用。
rem ============================================================================

setlocal
chcp 65001 >nul 2>&1
title UDP2CAL Build

set "PYTHONUTF8=1"
set "PYTHONIOENCODING=utf-8"
set "UDP2CAL_ROOT=%~dp0"

if not exist "%~dp0scripts\build.py" (
    echo [ERROR] 缺少 scripts\build.py，请确认脚本完整性
    pause
    exit /b 1
)

set "PY="

rem ---- 1) 用户显式指定 ----
if defined UDP2CAL_PYTHON (
    "%UDP2CAL_PYTHON%" -c "import sys;raise SystemExit(0 if sys.version_info>=(3,8) else 1)" >nul 2>&1
    if not errorlevel 1 set "PY="%UDP2CAL_PYTHON%""
)

rem ---- 2) py launcher ----
if not defined PY (
    py -3 -c "import sys;raise SystemExit(0 if sys.version_info>=(3,8) else 1)" >nul 2>&1
    if not errorlevel 1 set "PY=py -3"
)

rem ---- 3) PATH 中的 python / python3 ----
if not defined PY (
    python -c "import sys;raise SystemExit(0 if sys.version_info>=(3,8) else 1)" >nul 2>&1
    if not errorlevel 1 set "PY=python"
)
if not defined PY (
    python3 -c "import sys;raise SystemExit(0 if sys.version_info>=(3,8) else 1)" >nul 2>&1
    if not errorlevel 1 set "PY=python3"
)

rem ---- 4) 常见安装目录兜底（逐个校验版本，避免选中旧版 Python） ----
if not defined PY (
    for /d %%D in ("%LOCALAPPDATA%\Programs\Python\Python3*") do (
        if not defined PY call :try_python "%%D\python.exe"
    )
)
if not defined PY (
    for /d %%D in ("C:\Python3*") do (
        if not defined PY call :try_python "%%D\python.exe"
    )
)

if not defined PY (
    echo.
    echo ============================================================
    echo   [ERROR] 未找到 Python 3.8 或更高版本
    echo ============================================================
    echo.
    echo   安装方式（任选其一）:
    echo     winget install Python.Python.3.12
    echo     或从 https://www.python.org/downloads/ 安装
    echo.
    echo   安装时请勾选 "Add Python to PATH"。
    echo   若已安装但未在 PATH 中，可设置环境变量:
    echo     set UDP2CAL_PYTHON=D:\path\to\python.exe
    echo.
    pause
    exit /b 1
)

rem ---- 转发执行 ----
%PY% "%~dp0scripts\build.py" %*
exit /b %ERRORLEVEL%

rem ---- 子过程：校验候选 Python 版本后记录 ----
:try_python
if not exist "%~1" exit /b 0
"%~1" -c "import sys;raise SystemExit(0 if sys.version_info>=(3,8) else 1)" >nul 2>&1
if errorlevel 1 exit /b 0
set "PY="%~1""
exit /b 0
