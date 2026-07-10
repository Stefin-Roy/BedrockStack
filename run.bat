@echo off
REM BedrockOS - Quick Run
REM Runs QEMU with the prebuilt FAT32 image

set QEMU_PATH=C:\Program Files\qemu\qemu-system-x86_64.exe
set OVMF_CODE_SRC=C:\Program Files\qemu\share\edk2-x86_64-code.fd
set OVMF_VARS_SRC=C:\Program Files\qemu\share\edk2-x86_64-vars.fd
set OVMF_PATH=%~dp0target\ovmf_code.fd
set OVMF_VARS=%~dp0target\ovmf_vars.fd
set IMAGE_PATH=%~dp0target\os.img

if not exist "%IMAGE_PATH%" (
    echo ERROR: Disk image not found at %IMAGE_PATH%
    echo Run fullrun.py to build the image first.
    exit /b 1
)

REM Ensure firmware code is available in the workspace (avoid spaces in path)
if not exist "%OVMF_PATH%" copy /Y "%OVMF_CODE_SRC%" "%OVMF_PATH%" >nul

REM Copy a fresh, correctly-sized writable vars store from the matching template.
REM A fabricated blank file would not match the code image and OVMF may fail.
copy /Y "%OVMF_VARS_SRC%" "%OVMF_VARS%" >nul

echo Running QEMU with BedrockOS...
"%QEMU_PATH%" ^
    -drive if=pflash,format=raw,readonly=on,file="%OVMF_PATH%" ^
    -drive if=pflash,format=raw,file="%OVMF_VARS%" ^
    -drive format=raw,file="%IMAGE_PATH%" ^
    -m 256M ^
    -nographic ^
    -serial mon:stdio
