@echo off
setlocal EnableDelayedExpansion

REM ============================================================
REM  BedrockOS - Full Build + (Image) + QEMU  (debug, no TUI)
REM  Usage: fullrun.bat [arch]
REM    arch: x86_64 (default) | riscv64
REM  Logs everything to target\fullrun.log
REM ============================================================

set SCRIPT_DIR=%~dp0
set TARGET_DIR=%SCRIPT_DIR%target
set LOG_FILE=%TARGET_DIR%\fullrun.log
set QEMU_DIR=C:\Program Files\qemu

REM Parse architecture argument
set ARCH=%1
if "%ARCH%"=="" set ARCH=x86_64

if /i "%ARCH%"=="x86_64" goto :arch_x86_64
if /i "%ARCH%"=="riscv64" goto :arch_riscv64
echo [fullrun] ERROR: Unknown architecture "%ARCH%". Use x86_64 or riscv64.
exit /b 1

:arch_x86_64
set QEMU_PATH=%QEMU_DIR%\qemu-system-x86_64.exe
set OVMF_SOURCE=%QEMU_DIR%\share\edk2-x86_64-code.fd
set OVMF_VARS_SOURCE=%QEMU_DIR%\share\edk2-x86_64-vars.fd
if not exist "%OVMF_VARS_SOURCE%" set OVMF_VARS_SOURCE=%QEMU_DIR%\share\edk2-i386-vars.fd
set OVMF_PATH=%TARGET_DIR%\ovmf_code.fd
set OVMF_VARS=%TARGET_DIR%\ovmf_vars.fd
set IMAGE_PATH=%TARGET_DIR%\os.img
set NVME_IMAGE=%TARGET_DIR%\nvme.img

if not exist "%TARGET_DIR%" mkdir "%TARGET_DIR%"

REM Create a blank NVMe test disk image if missing
if not exist "%NVME_IMAGE%" (
    echo [fullrun] Creating NVMe test disk image...
    "%QEMU_DIR%\qemu-img" create -f raw "%NVME_IMAGE%" 64M >nul 2>&1
)

echo ============================================> "%LOG_FILE%"
echo  BedrockOS fullrun (x86_64) - %date% %time%>> "%LOG_FILE%"
echo ============================================>> "%LOG_FILE%"
echo [fullrun] Starting x86_64 build and run...
echo.

if not exist "%OVMF_PATH%" (
    echo [fullrun] Copying OVMF to workspace...
    copy /Y "%OVMF_SOURCE%" "%OVMF_PATH%" >nul
    if not exist "%OVMF_PATH%" (
        echo [fullrun] ERROR: Could not copy OVMF from %OVMF_SOURCE%
        exit /b 1
    )
)

echo [1/4] Building kernel (x86_64-unknown-none, debug)...
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

echo [2/4] Building boot (x86_64-unknown-uefi, debug)...
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

echo [3/4] Creating FAT32 disk image...
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

echo [4/4] Launching QEMU (x86_64)...
echo --- QEMU launch --- >> "%LOG_FILE%"
echo %date% %time% >> "%LOG_FILE%"
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
    -machine q35 ^
    -drive if=pflash,format=raw,readonly=on,file="%OVMF_PATH%" ^
    -drive if=pflash,format=raw,file="%OVMF_VARS%" ^
    -drive file="%IMAGE_PATH%",format=raw,if=none,id=disk0 ^
    -device ahci,id=ahci ^
    -device ide-hd,drive=disk0,bus=ahci.0 ^
    -drive file="%NVME_IMAGE%",format=raw,if=none,id=nvme_disk ^
    -device nvme,serial=1234,drive=nvme_disk ^
    -serial stdio ^
    -m 256M ^
    -smp 4
echo QEMU exited with code %errorlevel% >> "%LOG_FILE%"
echo [fullrun] QEMU exited with code %errorlevel%.
goto :done

:arch_riscv64
set QEMU_PATH=%QEMU_DIR%\qemu-system-riscv64.exe
set OPENSBI=%QEMU_DIR%\share\opensbi-riscv64-generic-fw_dynamic.bin
set KERNEL_PATH=%TARGET_DIR%\riscv64gc-unknown-none-elf\debug\kernel

if not exist "%TARGET_DIR%" mkdir "%TARGET_DIR%"

echo ============================================> "%LOG_FILE%"
echo  BedrockOS fullrun (riscv64) - %date% %time%>> "%LOG_FILE%"
echo ============================================>> "%LOG_FILE%"
echo [fullrun] Starting riscv64 build and run...
echo.

echo [1/2] Building kernel (riscv64gc-unknown-none-elf, debug)...
echo --- kernel build --- >> "%LOG_FILE%"
echo %date% %time% >> "%LOG_FILE%"
cargo build --target riscv64gc-unknown-none-elf -p kernel 2>&1
if %errorlevel% neq 0 (
    echo [fullrun] ERROR: kernel build failed with exit code %errorlevel%
    echo kernel build FAILED: exit %errorlevel% >> "%LOG_FILE%"
    exit /b 1
)
echo kernel build OK >> "%LOG_FILE%"
echo [fullrun] Kernel built successfully.
echo.

REM Verify kernel binary exists
if not exist "%KERNEL_PATH%" (
    echo [fullrun] WARNING: Kernel binary not found at expected path.
    echo Looking for kernel binary...
    where /R "%TARGET_DIR%" kernel 2>nul | findstr /V ".d"
    if errorlevel 1 (
        echo [fullrun] ERROR: Could not locate kernel binary.
        echo kernel not found >> "%LOG_FILE%"
        exit /b 1
    )
)

echo [2/2] Launching QEMU (riscv64)...
echo --- QEMU launch --- >> "%LOG_FILE%"
echo %date% %time% >> "%LOG_FILE%"

if not exist "%OPENSBI%" (
    echo [fullrun] WARNING: OpenSBI not found at %OPENSBI%
    echo OpenSBI not found, trying without -bios flag... >> "%LOG_FILE%"
)

if not exist "%QEMU_PATH%" (
    echo [fullrun] ERROR: QEMU not found at %QEMU_PATH%
    echo QEMU not found >> "%LOG_FILE%"
    exit /b 1
)

if exist "%OPENSBI%" (
    "%QEMU_PATH%" ^
        -machine virt ^
        -m 256M ^
        -kernel "%KERNEL_PATH%" ^
        -bios "%OPENSBI%" ^
        -nographic ^
        -serial mon:stdio ^
        -smp 4
) else (
    "%QEMU_PATH%" ^
        -machine virt ^
        -m 256M ^
        -kernel "%KERNEL_PATH%" ^
        -nographic ^
        -serial mon:stdio ^
        -smp 4
)
echo QEMU exited with code %errorlevel% >> "%LOG_FILE%"
echo [fullrun] QEMU exited with code %errorlevel%.
goto :done

:done
echo [fullrun] Log saved to %LOG_FILE%
