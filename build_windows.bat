@echo off
chcp 65001 >nul
echo ========================================
echo   UDP2Mic Windows 构建脚本
echo ========================================
echo.

pushd "%~dp0windows" || (echo Failed to change directory & exit /b 1)

echo [1/4] 检查 Rust 工具链...
rustc --version >nul 2>&1
if errorlevel 1 (
    echo [错误] 未检测到 Rust，请安装: https://rustup.rs
    pause
    exit /b 1
)
echo   Rust: OK

echo.
echo [2/4] 编译 Release...
set CMAKE_POLICY_VERSION_MINIMUM=3.5
cargo build --release 2>&1
if errorlevel 1 (
    echo [错误] 编译失败，请检查错误信息
    pause
    exit /b 1
)

echo.
echo [3/4] UPX 压缩...
set "UPX=%LOCALAPPDATA%\Microsoft\WinGet\Packages\UPX.UPX_Microsoft.Winget.Source_8wekyb3d8bbwe\upx-5.1.1-win64\upx.exe"
if exist "%UPX%" (
    "%UPX%" --best "target\release\udp2mic.exe" 2>&1
    if errorlevel 1 (
        echo   UPX 压缩失败，保留原始文件
    )
) else (
    where upx >nul 2>&1
    if not errorlevel 1 (
        upx --best "target\release\udp2mic.exe" 2>&1
    ) else (
        echo   UPX 未找到，跳过压缩
    )
)

echo.
echo [4/4] 复制到根目录...
copy /Y "target\release\udp2mic.exe" "%~dp0udp2mic.exe" >nul
if exist "%~dp0udp2mic.exe" (
    for %%A in ("%~dp0udp2mic.exe") do echo   根目录: %%~zA 字节
) else (
    echo [警告] 复制到根目录失败
)

echo.
echo [5/5] 检查输出...
if exist "target\release\udp2mic.exe" (
    for %%A in ("target\release\udp2mic.exe") do echo   大小: %%~zA 字节
    echo   ========================================
    echo   构建成功！
    echo   输出: target\release\udp2mic.exe
    echo   根目录: %~dp0udp2mic.exe
    echo   ========================================
) else (
    echo [错误] 未找到输出文件
)

popd
pause
