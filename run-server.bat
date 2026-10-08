@echo off
setlocal EnableExtensions EnableDelayedExpansion
rem Agent2API headless gateway - local startup script
rem Usage:
rem   run-server.bat
rem   run-server.bat -BindHost 0.0.0.0
rem   run-server.bat -Port 8080 -VerboseLog

set "BindHost=127.0.0.1"
set "Port=3065"
set "VerboseLog=0"

:parse_args
if "%~1"=="" goto args_done
if /I "%~1"=="-BindHost" (
    if "%~2"=="" (
        echo Missing value for -BindHost.
        exit /b 2
    )
    set "BindHost=%~2"
    shift
    shift
    goto parse_args
)
if /I "%~1"=="-Port" (
    if "%~2"=="" (
        echo Missing value for -Port.
        exit /b 2
    )
    set "Port=%~2"
    shift
    shift
    goto parse_args
)
if /I "%~1"=="-VerboseLog" (
    set "VerboseLog=1"
    shift
    goto parse_args
)
echo Unknown argument: %~1
echo Usage: %~nx0 [-BindHost host] [-Port port] [-VerboseLog]
exit /b 2

:args_done
set "ROOT=%~dp0"
set "EXE=%ROOT%target\release\agent2api-server.exe"
set "DATA_DIR=%ROOT%data"
set "UI_DIR=%ROOT%desktop-tauri\ui"

if not exist "%EXE%" (
    echo Server executable not found:
    echo   %EXE%
    echo Please run: cargo build --release -p agent2api-server
    exit /b 1
)

set "AGENT2API_PROXY_HOME=%DATA_DIR%"
set "AGENT2API_UI_DIR=%UI_DIR%"
set "AGENT2API_HOST=%BindHost%"
set "AGENT2API_PROXY_PORT=%Port%"
if "%VerboseLog%"=="1" set "AGENT2API_VERBOSE=1"

if not exist "%AGENT2API_PROXY_HOME%" mkdir "%AGENT2API_PROXY_HOME%"
if not exist "%AGENT2API_UI_DIR%\index.html" (
    echo Warning: UI file not found: %AGENT2API_UI_DIR%\index.html
)

echo Panel  http://%BindHost%:%Port%/
echo Gateway http://%BindHost%:%Port%/v1
echo Data   %AGENT2API_PROXY_HOME%
echo UI     %AGENT2API_UI_DIR%
echo.

"%EXE%"
set "EXIT_CODE=%ERRORLEVEL%"
endlocal & exit /b %EXIT_CODE%
