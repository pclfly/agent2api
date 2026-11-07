@echo off
REM ===========================================================================
REM  Agent2API server-only build (Windows)
REM
REM  Usage:
REM    build-server.bat             build the release server binary
REM    build-server.bat --check     type-check only (cargo check, fast)
REM    build-server.bat --test      run server unit tests
REM    build-server.bat --nolock    pass through without --locked
REM    build-server.bat --help      show this text
REM
REM  --locked (the default) makes cargo refuse to touch Cargo.lock. That is what
REM  you want normally: it proves the build uses exactly the versions pinned in
REM  the repo. It fails with "the lock file needs to be updated" whenever a
REM  Cargo.toml dependency was edited without refreshing the lock. When you have
REM  deliberately bumped a dependency, run --nolock once to let cargo rewrite
REM  the lock, then commit Cargo.lock.
REM
REM  Why server only
REM  ---------------
REM  The local deployment is a headless server (panel served over HTTP); no
REM  desktop shell or installer is produced here. -p agent2api-server
REM  --bin agent2api-server keeps the tauri dependency tree out of the build.
REM
REM  Artifact
REM  --------
REM    target\release\agent2api-server.exe
REM  NOT desktop-tauri\src-tauri\target\release\. The workspace pins
REM  target-dir = "../../target" in desktop-tauri\src-tauri\.cargo\config.toml,
REM  which resolves to this repo root. start-plugin-server.ps1 launches
REM  from the same path, so keep the two in sync if either moves.
REM
REM  Run after building
REM  ------------------
REM    powershell -ExecutionPolicy Bypass -File .\start-plugin-server.ps1
REM
REM  Encoding contract: ASCII text, CRLF line endings, nothing else.
REM  cmd.exe mis-parses UTF-8 or GBK bytes in a batch file, and it also
REM  mis-parses LF-only line endings. Either one makes the parser choke on the
REM  first REM line, the window closes, and the failure is invisible. Do not
REM  put non-ASCII characters here and do not let an editor save it as LF.
REM ===========================================================================
setlocal EnableExtensions EnableDelayedExpansion

cd /d "%~dp0"

REM Capture the script name before any shift. shift moves %0 along with the
REM arguments, so a later %~nx0 would name whatever argument landed in slot 0
REM (e.g. "--check") instead of this file.
set "ME=%~nx0"

set "MANIFEST=desktop-tauri\src-tauri\Cargo.toml"
set "EXE=target\release\agent2api-server.exe"
set "PKG=-p agent2api-server"
set "BIN=--bin agent2api-server"

REM A double-clicked batch runs in a console that closes as soon as the script
REM ends, so a failure flashes past unseen. The invoking command line contains
REM this script's own name in that case, so pause at the end to keep it visible.
REM Launched from an existing console the name is not there and nothing pauses.
REM
REM Pure-batch substring test on purpose: shelling out to find.exe would be
REM resolved against the caller's PATH, where a POSIX find earlier in PATH
REM takes /i as a path and fails.
set "DO_PAUSE=0"
set "_CMD=!cmdcmdline!"
if not "!_CMD:%ME%=!"=="!_CMD!" set "DO_PAUSE=1"
REM Automation escape hatch, e.g. CI or a wrapper script.
if defined AGENT2API_NO_PAUSE set "DO_PAUSE=0"

set "MODE="
set "NOLOCK="

:parse
REM Whitespace-only arguments are ignored, not rejected. Explorer launches a
REM double-clicked .bat as: cmd /c ""C:\path\build-server.bat" " -- note the
REM trailing quoted space. cmd hands that to the script as a real first
REM argument whose value is a single space, which fails the =="" test, hits
REM the unknown-argument branch, and closes the window with exit code 2. Trim
REM surrounding spaces first and treat an empty result as "no argument".
set "_A=%~1"
if "%~1"=="" goto parse_done
if "!_A: =!"=="" shift & goto parse
if /i "%~1"=="--check"  goto arg_check
if /i "%~1"=="--test"   goto arg_test
if /i "%~1"=="--nolock" goto arg_nolock
if /i "%~1"=="--unlock" goto arg_nolock
if /i "%~1"=="--help"   goto mode_help
if /i "%~1"=="-h"       goto mode_help
if /i "%~1"=="/?"       goto mode_help
echo [ERROR] Unknown argument: [%~1]
goto usage

:arg_check
if defined MODE goto arg_conflict
set "MODE=check"
shift
goto parse

:arg_test
if defined MODE goto arg_conflict
set "MODE=test"
shift
goto parse

:arg_nolock
set "NOLOCK=1"
shift
goto parse

:arg_conflict
echo [ERROR] Only one mode per run: got %~1 after --%MODE%
goto usage

:mode_help
set "MODE=help"
set "RC=0"
goto usage

:parse_done
if not defined MODE set "MODE=build"
set "LOCKARG=--locked"
if defined NOLOCK (
    set "LOCKARG="
    echo [WARN] --locked disabled for this run: cargo may rewrite Cargo.lock.
    echo        Commit the refreshed lock file when a dependency bump is intended.
    echo.
)
if "%MODE%"=="check" goto check
if "%MODE%"=="test"  goto test
goto build

:usage
echo Usage: %ME% [--check ^| --test] [--nolock] [--help]
echo.
echo   (no flag)  build the release server binary
echo   --check    type-check only, no codegen
echo   --test     run server unit tests
echo   --nolock   do not pass --locked to cargo
echo   --help     show this text
if not defined RC set "RC=2"
goto done

:check
echo [1/1] cargo check %PKG% %LOCKARG%
cargo check --manifest-path "%MANIFEST%" %PKG% %LOCKARG%
if errorlevel 1 goto fail
echo.
echo Check passed.
set "RC=0"
goto done

:test
echo [1/1] cargo test %PKG% %LOCKARG%
cargo test --manifest-path "%MANIFEST%" %PKG% --lib %LOCKARG%
if errorlevel 1 goto fail
echo.
echo Tests passed.
set "RC=0"
goto done

:build
echo [1/2] Building release server, about 4 minutes...
cargo build --manifest-path "%MANIFEST%" %PKG% %BIN% --release %LOCKARG%
if errorlevel 1 goto fail

echo.
echo [2/2] Verifying artifact
if not exist "%EXE%" (
    echo [ERROR] Artifact missing: %EXE%
    goto fail
)
for %%F in ("%EXE%") do echo     %%F  %%~zF bytes
echo.
echo Done. Start it with:
echo   powershell -ExecutionPolicy Bypass -File .\start-plugin-server.ps1
set "RC=0"
goto done

:fail
set "RC=%ERRORLEVEL%"
if "%RC%"=="0" set "RC=1"
echo.
echo [ERROR] %MODE% failed with exit code %RC%.
if defined LOCKARG (
    echo.
    echo If the output above says the lock file needs to be updated, a Cargo.toml
    echo dependency changed without refreshing Cargo.lock. Bump it once with:
    echo   %ME% --nolock
    echo then commit Cargo.lock. Using --nolock permanently is not a fix.
)
goto done

:done
if "%DO_PAUSE%"=="1" pause
endlocal & exit /b %RC%
