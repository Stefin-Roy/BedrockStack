@echo off
setlocal EnableDelayedExpansion
REM BedrockOS - Quick Run
REM Usage: run.bat [arch]
REM   arch: x86_64 (default) | riscv64

set QEMU_DIR=C:\Program Files\qemu

set ARCH=%1
if "%ARCH%"=="" set ARCH=x86_64

if /i "%ARCH%"=="x86_64" goto :arch_x86_64
if /i "%ARCH%"=="riscv64" goto :arch_riscv64
echo ERROR: Unknown architecture "%ARCH%". Use x86_64 or riscv64.
exit /b 1

:arch_x86_64
set QEMU_PATH=%QEMU_DIR%\qemu-system-x86_64.exe
set OVMF_CODE_SRC=%QEMU_DIR%\share\edk2-x86_64-code.fd
set OVMF_VARS_SRC=%QEMU_DIR%\share\edk2-x86_64-vars.fd
set OVMF_PATH=%~dp0target\ovmf_code.fd
set OVMF_VARS=%~dp0target\ovmf_vars.fd
set IMAGE_PATH=%~dp0target\os.img

if not exist "%IMAGE_PATH%" (
    echo ERROR: Disk image not found at %IMAGE_PATH%
    echo Run fullrun.bat to build the image first.
    exit /b 1
)
if not exist "%OVMF_PATH%" copy /Y "%OVMF_CODE_SRC%" "%OVMF_PATH%" >nul
copy /Y "%OVMF_VARS_SRC%" "%OVMF_VARS%" >nul
echo Running QEMU with BedrockOS (x86_64^)...
"%QEMU_PATH%" ^
    -drive if=pflash,format=raw,readonly=on,file="%OVMF_PATH%" ^
    -drive if=pflash,format=raw,file="%OVMF_VARS%" ^
    -drive format=raw,file="%IMAGE_PATH%" ^
    -m 256M ^
    -nographic ^
    -serial mon:stdio
goto :done

:arch_riscv64
set QEMU_PATH=%QEMU_DIR%\qemu-system-riscv64.exe
set OPENSBI=%QEMU_DIR%\share\opensbi-riscv64-generic-fw_dynamic.bin
set KERNEL_PATH=%~dp0target\riscv64gc-unknown-none-elf\debug\kernel

if not exist "%KERNEL_PATH%" (
    echo ERROR: RISC-V kernel binary not found at %KERNEL_PATH%
    echo Run fullrun.bat riscv64 to build it first.
    exit /b 1
)
if not exist "%QEMU_PATH%" (
    echo ERROR: QEMU riscv64 not found at %QEMU_PATH%
    exit /b 1
)
echo Running QEMU with BedrockOS (riscv64^)...
if exist "%OPENSBI%" (
    "%QEMU_PATH%" ^
        -machine virt ^
        -m 256M ^
        -kernel "%KERNEL_PATH%" ^
        -bios "%OPENSBI%" ^
        -nographic ^
        -serial mon:stdio
) else (
    "%QEMU_PATH%" ^
        -machine virt ^
        -m 256M ^
        -kernel "%KERNEL_PATH%" ^
        -nographic ^
        -serial mon:stdio
)
goto :done

:done
