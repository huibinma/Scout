@echo off
setlocal
rem ---------------------------------------------------------------------------
rem Build scoutd with the llama-cpp feature (real embedder) on Windows.
rem
rem Usage:   scripts\build-scoutd-llama.bat [extra cargo args, e.g. --release]
rem
rem Prerequisites (each overridable via env var):
rem   1. VS 2022 Build Tools (MSVC + vcvars64.bat)     -> SCOUT_VCVARS
rem   2. libclang (LLVM Windows release, for bindgen)  -> LIBCLANG_PATH
rem      Default: <repo>\.tmp\LLVM-20.1.8-extracted\bin
rem   3. cmake + ninja: taken from VS Build Tools' bundled copies; no separate
rem      install needed.
rem
rem llcb cache note: llama-cpp-sys-4 (0.3.x) on Windows redirects its cmake
rem build tree to %LOCALAPPDATA%\llcb\<OUT_DIR hash> (MAX_PATH workaround).
rem This script points LOCALAPPDATA at <repo>\.tmp (override with
rem SCOUT_LLCB_HOME) so the cache lives with the repo and is reused across
rem builds: warm rebuild ~2 min instead of ~10 min cold. Keep the same value
rem across runs or the cache misses.
rem ---------------------------------------------------------------------------

rem Repo root = parent of this script's directory (normalized).
for %%I in ("%~dp0..") do set "REPO_ROOT=%%~fI"

rem --- 1) MSVC toolchain -----------------------------------------------------
if not defined SCOUT_VCVARS set "SCOUT_VCVARS=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
if not exist "%SCOUT_VCVARS%" (
    echo [error] vcvars64.bat not found: "%SCOUT_VCVARS%"
    echo         Install VS 2022 Build Tools, or set SCOUT_VCVARS to your vcvars64.bat.
    exit /b 1
)
call "%SCOUT_VCVARS%"
if errorlevel 1 exit /b 1

rem --- 2) cmake + ninja from VS Build Tools ----------------------------------
set "VS_ROOT=%SCOUT_VCVARS:\VC\Auxiliary\Build\vcvars64.bat=%"
set "PATH=%VS_ROOT%\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin;%VS_ROOT%\Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja;%PATH%"
where cmake >nul 2>nul
if errorlevel 1 (
    echo [error] cmake not found on PATH ^(looked under "%VS_ROOT%\Common7\...\CMake"^).
    echo         Install the "C++ CMake tools for Windows" Build Tools component,
    echo         or put your own cmake on PATH before running this script.
    exit /b 1
)
where ninja >nul 2>nul
if errorlevel 1 (
    echo [error] ninja not found on PATH. Same fix as cmake above.
    exit /b 1
)

rem --- 3) libclang for bindgen -------------------------------------------------
if not defined LIBCLANG_PATH (
    if exist "%REPO_ROOT%\.tmp\LLVM-20.1.8-extracted\bin\libclang.dll" (
        set "LIBCLANG_PATH=%REPO_ROOT%\.tmp\LLVM-20.1.8-extracted\bin"
    )
)
if not defined LIBCLANG_PATH (
    echo [error] LIBCLANG_PATH not set and no bundled copy at
    echo         "%REPO_ROOT%\.tmp\LLVM-20.1.8-extracted\bin".
    echo         Download an LLVM Windows release ^(e.g. clang+llvm-*-x86_64-pc-windows-msvc.tar.xz^),
    echo         extract it, and set LIBCLANG_PATH to its bin\ directory.
    exit /b 1
)

rem --- 4) llcb cache home ------------------------------------------------------
if not defined SCOUT_LLCB_HOME set "SCOUT_LLCB_HOME=%REPO_ROOT%\.tmp"
if not exist "%SCOUT_LLCB_HOME%" mkdir "%SCOUT_LLCB_HOME%"
set "LOCALAPPDATA=%SCOUT_LLCB_HOME%"

set "CMAKE_GENERATOR=Ninja"

echo [info] LIBCLANG_PATH = %LIBCLANG_PATH%
echo [info] llcb cache    = %LOCALAPPDATA%\llcb
cd /d "%REPO_ROOT%"
cargo build -p scoutd --features scout-model-runtime/llama-cpp %*
