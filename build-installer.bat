@echo off
setlocal EnableExtensions DisableDelayedExpansion
rem Historical filename retained; this entry builds only agent2api-server.
rem ASCII keeps this file compatible with both CP936 and UTF-8 cmd consoles.
set "ROOT=%~dp0"
set "OUT=%ROOT%target\release\agent2api-server.exe"
set "DO_PAUSE=1"
set "CHECK_ONLY=0"
set "RC=1"

:parse_args
if "%~1"=="" goto args_done
if /I "%~1"=="-nopause" (
    set "DO_PAUSE=0"
    shift
    goto parse_args
)
if /I "%~1"=="--check" (
    set "CHECK_ONLY=1"
    shift
    goto parse_args
)
if /I "%~1"=="--help" goto usage
if /I "%~1"=="-h" goto usage
if /I "%~1"=="clean" goto clean_disabled
echo [ERROR] Unknown argument: %~1
set "RC=2"
goto finish

:usage
echo Usage: %~nx0 [--check] [-nopause]
echo Default: build the release server only. No desktop app or installer.
echo --check: validate server code without linking an executable.
echo Output: target\release\agent2api-server.exe
echo Start:  run-server.bat
endlocal & exit /b 0

:clean_disabled
echo [ERROR] The legacy clean option is disabled to protect the running server.
echo Cache cleanup must preserve the server executable and be done separately.
endlocal & exit /b 2

:args_done
cd /d "%ROOT%"
if errorlevel 1 goto finish
where cargo >nul 2>nul
if not errorlevel 1 goto tools_ready
if exist "%USERPROFILE%\.cargo\bin\cargo.exe" set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
:tools_ready
where cargo >nul 2>nul
if errorlevel 1 goto no_rust
where rustc >nul 2>nul
if errorlevel 1 goto no_rust
if not exist "%ROOT%desktop-tauri\ui\index.html" (
    echo [ERROR] Shared server web UI is missing: desktop-tauri\ui\index.html
    goto finish
)
set "CARGO_TARGET_DIR=%ROOT%target"
set "CARGO_INCREMENTAL=0"
echo ============================================================
echo Agent2API server-only build
call cargo --version
if errorlevel 1 goto finish
echo Output: %OUT%
echo Desktop, Tauri, NSIS and npm installation are skipped.
echo ============================================================
if "%CHECK_ONLY%"=="1" goto check_server

rem Windows locks a running EXE; never stop the service implicitly.
set "AGENT2API_BUILD_EXE=%OUT%"
powershell.exe -NoProfile -Command "$ErrorActionPreference='Stop'; try { $p=Get-CimInstance Win32_Process -Filter \"Name='agent2api-server.exe'\"; if ($p | Where-Object { -not $_.ExecutablePath -or $_.ExecutablePath -eq $env:AGENT2API_BUILD_EXE }) { Write-Host '[ERROR] Server executable is running. Stop it manually before rebuilding, or use --check.'; exit 3 }; exit 0 } catch { Write-Host $_; exit 1 }"
set "RC=%ERRORLEVEL%"
if not "%RC%"=="0" goto finish
call cargo build --manifest-path "%ROOT%desktop-tauri\src-tauri\Cargo.toml" -p agent2api-server --bin agent2api-server --release --locked
set "RC=%ERRORLEVEL%"
if not "%RC%"=="0" goto finish
if not exist "%OUT%" (
    echo [ERROR] Cargo succeeded but the server executable is missing.
    set "RC=1"
    goto finish
)
echo [OK] Server built: %OUT%
echo Start with: "%ROOT%run-server.bat"
echo Keep desktop-tauri\ui and data; no desktop installer is generated.
goto finish

:check_server
call cargo check --manifest-path "%ROOT%desktop-tauri\src-tauri\Cargo.toml" -p agent2api-server --locked
set "RC=%ERRORLEVEL%"
if "%RC%"=="0" echo [OK] Server check passed.
goto finish

:no_rust
echo [ERROR] Rust 1.88 or newer and MSVC C++ build tools are required.
set "RC=1"

:finish
if not "%RC%"=="0" echo [ERROR] Server build/check failed with exit code %RC%.
if "%DO_PAUSE%"=="1" pause
endlocal & exit /b %RC%
