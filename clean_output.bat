@echo off
set "P1=android\app\build\outputs\apk\release"
set "P2=windows\target\release"

if exist "%P1%" (
    del /f /s /q "%P1%\*.*" >nul
    for /d %%d in ("%P1%\*") do rd /s /q "%%d"
)

if exist "%P2%" (
    del /f /s /q "%P2%\*.*" >nul
    for /d %%d in ("%P2%\*") do rd /s /q "%%d"
)

echo Done.
pause