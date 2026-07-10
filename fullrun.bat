@echo off
setlocal EnableDelayedExpansion

REM ============================================================
REM  BedrockOS - Full Build + Image + QEMU  (debug, no TUI)
REM  Logs everything to target\fullrun.log
REM ============================================================

REM NOTE: use BACKSLASHES in Windows paths. cmd's `copy` treats `/` as a switch
REM delimiter, so forward-slash paths (even quoted) fail to copy.
set SCRIPT_DIR=%~dp0
set TARGET_DIR=%SCRIPT_DIR%target
set LOG_FILE=%TARGET_DIR%\fullrun.log
set QEMU_DIR=C:\Program Files\qemu
set QEMU_PATH=%QEMU_DIR%\qemu-system-x86_64.exe
set OVMF_SOURCE=%QEMU_DIR%\share\edk2-x86_64-code.fd
REM This QEMU build ships no edk2-x86_64-vars.fd. The varstore FV is
REM architecture-independent, and edk2-i386-vars.fd is the same empty store at
REM the correct size (code 3653632 + vars 540672 = 4 MiB). Prefer the x86_64
REM vars file if a future install provides it, else fall back to the i386 one.
set OVMF_VARS_SOURCE=%QEMU_DIR%\share\edk2-x86_64-vars.fd
if not exist "%OVMF_VARS_SOURCE%" set OVMF_VARS_SOURCE=%QEMU_DIR%\share\edk2-i386-vars.fd
set OVMF_PATH=%~dp0target\ovmf_code.fd
set OVMF_VARS=%~dp0target\ovmf_vars.fd
set IMAGE_PATH=%TARGET_DIR%\os.img

if not exist "%TARGET_DIR%" mkdir "%TARGET_DIR%"

REM Clear log
echo ============================================> "%LOG_FILE%"
echo  BedrockOS fullrun - %date% %time%>> "%LOG_FILE%"
echo ============================================>> "%LOG_FILE%"

echo [fullrun] Starting build and run...
echo.

REM ---- Step 0: Copy OVMF to workspace (avoid spaces in path) ----
if not exist "%OVMF_PATH%" (
    echo [fullrun] Copying OVMF to workspace...
    copy /Y "%OVMF_SOURCE%" "%OVMF_PATH%" >nul
    if not exist "%OVMF_PATH%" (
        echo [fullrun] ERROR: Could not copy OVMF from %OVMF_SOURCE%
        exit /b 1
    )
)

REM ---- Step 1: Build kernel ----
echo [1/3] Building kernel (x86_64-unknown-none, debug^)...
echo --- kernel build --- >> "%LOG_FILE%"
echo %date% %time% >> "%LOG_FILE%"

cargo build --target x86_64-unknown-none -p kernel 2>&1
if %errorlevel% neq 0 (
    echo [fullrun] ERROR: kernel build failed with exit code %errorlevel%
    echo kernel build FAILED: exit %errorlevel% >> "%LOG_FILE%"
    exit /b 1
)
echo kernel build OK >> "%LOG_FILE%"
echo [fullrun] Kernel built successfully.
echo.

REM ---- Step 2: Build boot (UEFI app) ----
echo [2/3] Building boot (x86_64-unknown-uefi, debug^)...
echo --- boot build --- >> "%LOG_FILE%"
echo %date% %time% >> "%LOG_FILE%"

cargo build --target x86_64-unknown-uefi -p boot 2>&1
if %errorlevel% neq 0 (
    echo [fullrun] ERROR: boot build failed with exit code %errorlevel%
    echo boot build FAILED: exit %errorlevel% >> "%LOG_FILE%"
    exit /b 1
)
echo boot build OK >> "%LOG_FILE%"
echo [fullrun] Boot built successfully.
echo.

REM ---- Step 3: Create FAT32 disk image ----
echo [3/3] Creating FAT32 disk image...
echo --- image creation --- >> "%LOG_FILE%"
echo %date% %time% >> "%LOG_FILE%"

python "%SCRIPT_DIR%create_image.py" 2>&1
if %errorlevel% neq 0 (
    echo [fullrun] ERROR: image creation failed with exit code %errorlevel%
    echo image creation FAILED: exit %errorlevel% >> "%LOG_FILE%"
    exit /b 1
)

if not exist "%IMAGE_PATH%" (
    echo [fullrun] ERROR: Disk image not found at %IMAGE_PATH%
    echo image file missing >> "%LOG_FILE%"
    exit /b 1
)
echo image creation OK >> "%LOG_FILE%"
echo [fullrun] Disk image created.
echo.

REM ---- Step 4: Launch QEMU ----
echo [fullrun] Launching QEMU...
echo --- QEMU launch --- >> "%LOG_FILE%"
echo %date% %time% >> "%LOG_FILE%"

REM Copy a fresh, correctly-sized writable vars store from the matching
REM template so OVMF re-discovers boot entries. A fabricated blank file would
REM not match the code image and OVMF may fail to boot.
if not exist "%OVMF_VARS_SOURCE%" (
    echo [fullrun] ERROR: OVMF vars template not found: %OVMF_VARS_SOURCE%
    echo OVMF vars template missing >> "%LOG_FILE%"
    exit /b 1
)
copy /Y "%OVMF_VARS_SOURCE%" "%OVMF_VARS%" >nul

if not exist "%QEMU_PATH%" (
    echo [fullrun] ERROR: QEMU not found at %QEMU_PATH%
    echo QEMU not found >> "%LOG_FILE%"
    exit /b 1
)

"%QEMU_PATH%" ^
    -drive if=pflash,format=raw,readonly=on,file="%OVMF_PATH%" ^
    -drive if=pflash,format=raw,file="%OVMF_VARS%" ^
    -drive format=raw,file="%IMAGE_PATH%" ^
    -serial stdio ^
    -m 256M

echo.
echo QEMU exited with code %errorlevel% >> "%LOG_FILE%"
echo [fullrun] QEMU exited with code %errorlevel%.
echo [fullrun] Log saved to %LOG_FILE%
