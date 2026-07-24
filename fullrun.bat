@echo off
setlocal EnableDelayedExpansion

REM ============================================================
REM  BedrockOS - Full Build + (Image) + QEMU  (debug, no TUI)
REM  Usage: fullrun.bat [arch] [boot_mode]
REM    arch:      x86_64 (default) | riscv64
REM    boot_mode: uefi (default) | grub
REM              grub: builds kernel with kernelmb2 feature, creates
REM                    GRUB standalone UEFI image via WSL, boots via
REM                    multiboot2 (x86_64 only)
REM  Logs everything to target\fullrun.log
REM ============================================================

set SCRIPT_DIR=%~dp0
set TARGET_DIR=%SCRIPT_DIR%target
set LOG_FILE=%TARGET_DIR%\fullrun.log
set QEMU_DIR=C:\Program Files\qemu

REM Parse arguments
set ARCH=%1
if "%ARCH%"=="" set ARCH=x86_64
set BOOT_MODE=%2
if "%BOOT_MODE%"=="" set BOOT_MODE=uefi

REM Set to 1 to gate the CPU slow mode feature (Intel-only, x86_64 only).
set CPU_SLOW=1

if /i "%ARCH%"=="x86_64" if /i "%BOOT_MODE%"=="grub" goto :arch_x86_64_grub
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
set CARGO_FEATURES=--features display_log
if "%CPU_SLOW%"=="1" set CARGO_FEATURES=--features "display_log cpu_slow"
cargo build --target x86_64-unknown-none -p kernel %CARGO_FEATURES% 2>&1
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
set CARGO_FEATURES=
if "%CPU_SLOW%"=="1" set CARGO_FEATURES=--features cpu_slow
cargo build --target x86_64-unknown-uefi -p boot %CARGO_FEATURES% 2>&1
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
    -netdev user,id=net0,hostfwd=tcp::8080-:80 ^
    -device virtio-net,netdev=net0 ^
    -serial stdio ^
    -m 7120M ^
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
cargo build --target riscv64gc-unknown-none-elf -p kernel --features display_log 2>&1
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

:arch_x86_64_grub
set QEMU_PATH=%QEMU_DIR%\qemu-system-x86_64.exe
set OVMF_SOURCE=%QEMU_DIR%\share\edk2-x86_64-code.fd
set OVMF_VARS_SOURCE=%QEMU_DIR%\share\edk2-x86_64-vars.fd
if not exist "%OVMF_VARS_SOURCE%" set OVMF_VARS_SOURCE=%QEMU_DIR%\share\edk2-i386-vars.fd
set OVMF_PATH=%TARGET_DIR%\ovmf_code.fd
set OVMF_VARS=%TARGET_DIR%\ovmf_vars.fd
set IMAGE_PATH=%TARGET_DIR%\os.img
set NVME_IMAGE=%TARGET_DIR%\nvme.img
set GRUB_CFG=%TARGET_DIR%\grub.cfg
set GRUB_EFI=%TARGET_DIR%\grub_bootx64.efi

if not exist "%TARGET_DIR%" mkdir "%TARGET_DIR%"

if not exist "%NVME_IMAGE%" (
    echo [fullrun] Creating NVMe test disk image...
    "%QEMU_DIR%\qemu-img" create -f raw "%NVME_IMAGE%" 64M >nul 2>&1
)

echo ============================================> "%LOG_FILE%"
echo  BedrockOS fullrun (x86_64, GRUB+Multiboot2) - %date% %time%>> "%LOG_FILE%"
echo ============================================>> "%LOG_FILE%"
echo [fullrun] Starting x86_64 GRUB+Multiboot2 build and run...
echo.

if not exist "%OVMF_PATH%" (
    echo [fullrun] Copying OVMF to workspace...
    copy /Y "%OVMF_SOURCE%" "%OVMF_PATH%" >nul
    if not exist "%OVMF_PATH%" (
        echo [fullrun] ERROR: Could not copy OVMF from %OVMF_SOURCE%
        exit /b 1
    )
)

echo [1/3] Building kernel (x86_64-unknown-none, debug, kernelmb2)...
echo --- kernel build --- >> "%LOG_FILE%"
echo %date% %time% >> "%LOG_FILE%"
set CARGO_FEATURES=--features "display_log kernelmb2"
if "%CPU_SLOW%"=="1" set CARGO_FEATURES=--features "display_log kernelmb2"
cargo build --target x86_64-unknown-none -p kernel %CARGO_FEATURES% 2>&1
if %errorlevel% neq 0 (
    echo [fullrun] ERROR: kernel build failed with exit code %errorlevel%
    echo kernel build FAILED: exit %errorlevel% >> "%LOG_FILE%"
    exit /b 1
)
echo kernel build OK >> "%LOG_FILE%"
echo [fullrun] Kernel built successfully.
echo.

echo [2/3] Creating GRUB standalone image via WSL...
echo --- grub-mkstandalone --- >> "%LOG_FILE%"
echo %date% %time% >> "%LOG_FILE%"

REM Convert TARGET_DIR to WSL path (used below; compute once outside the if block)
set "WSL_TARGET=%TARGET_DIR:\=/%"
set DRIVE_LETTER=%WSL_TARGET:~0,1%
for %%a in (a b c d e f g h i j k l m n o p q r s t u v w x y z) do if /i "%%a"=="%DRIVE_LETTER%" set DRIVE_LETTER=%%a
set "WSL_TARGET=/mnt/%DRIVE_LETTER%%WSL_TARGET:~2%"

REM Check GRUB cache — only regenerate if the config changed
set GRUB_CFG_CACHED=%GRUB_CFG%.cached
set GRUB_SKIP=0
if exist "%GRUB_EFI%" if exist "%GRUB_CFG_CACHED%" (
    > "%TARGET_DIR%\_grub_cmp.cfg" (
    echo set timeout=1
    echo set default=0
    echo insmod efi_gop
    echo insmod video
    echo insmod all_video
    echo set gfxmode=1024x768x32
    echo set gfxpayload=keep
    echo.
    echo menuentry "BedrockOS" {
    echo     insmod part_gpt
    echo     insmod fat
    echo     search --no-floppy --set=root --file /EFI/BEDROCK/KERNEL
    echo     multiboot2 /EFI/BEDROCK/KERNEL
    echo     boot
    echo }
    )
    fc "%TARGET_DIR%\_grub_cmp.cfg" "%GRUB_CFG_CACHED%" >nul 2>&1
    if !errorlevel! equ 0 set GRUB_SKIP=1
    del "%TARGET_DIR%\_grub_cmp.cfg"
)

if %GRUB_SKIP% equ 1 (
    echo [fullrun] GRUB config unchanged, reusing cached %GRUB_EFI%>> "%LOG_FILE%"
    echo [fullrun] GRUB config unchanged, reusing cached image.
) else (
    echo [fullrun] GRUB config changed or missing, regenerating...>> "%LOG_FILE%"
    echo [fullrun] GRUB config changed or missing, regenerating...

    REM Write grub.cfg
    (
    echo set timeout=1
    echo set default=0
    echo insmod efi_gop
    echo insmod video
    echo insmod all_video
    echo set gfxmode=1024x768x32
    echo set gfxpayload=keep
    echo.
    echo menuentry "BedrockOS" {
    echo     insmod part_gpt
    echo     insmod fat
    echo     search --no-floppy --set=root --file /EFI/BEDROCK/KERNEL
    echo     multiboot2 /EFI/BEDROCK/KERNEL
    echo     boot
    echo }
    ) > "%GRUB_CFG%"

    REM Update cache
    copy /Y "%GRUB_CFG%" "%GRUB_CFG_CACHED%" >nul

    REM Install grub-efi-amd64-bin if missing, then run grub-mkstandalone
    wsl bash -c "set -euo pipefail; if ! command -v grub-mkstandalone >/dev/null 2>&1; then sudo apt-get update -qq && sudo apt-get install -y -qq grub-efi-amd64-bin; fi; grub-mkstandalone -O x86_64-efi -o '%WSL_TARGET%/grub_bootx64.efi' --modules='part_gpt fat multiboot2 video efi_gop all_video gfxterm' 'boot/grub/grub.cfg=%WSL_TARGET%/grub.cfg'"
    if !errorlevel! neq 0 (
        echo [fullrun] ERROR: grub-mkstandalone failed with exit code !errorlevel!
        echo grub-mkstandalone FAILED: exit !errorlevel! >> "%LOG_FILE%"
        exit /b 1
    )
    echo grub-mkstandalone OK >> "%LOG_FILE%"
)

echo [fullrun] GRUB standalone image ready.
echo.

echo [3/3] Creating disk image...
echo --- image creation --- >> "%LOG_FILE%"
echo %date% %time% >> "%LOG_FILE%"

REM Place GRUB EFI where create_image.py expects boot.efi
if not exist "%TARGET_DIR%\x86_64-unknown-uefi\debug" mkdir "%TARGET_DIR%\x86_64-unknown-uefi\debug"
copy /Y "%GRUB_EFI%" "%TARGET_DIR%\x86_64-unknown-uefi\debug\boot.efi" >nul

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
    -netdev user,id=net0,hostfwd=tcp::8080-:80 ^
    -device virtio-net,netdev=net0 ^
    -vga std ^
    -serial stdio ^
    -m 7120M ^
    -smp 4
echo QEMU exited with code %errorlevel% >> "%LOG_FILE%"
echo [fullrun] QEMU exited with code %errorlevel%.
goto :done

:done
echo [fullrun] Log saved to %LOG_FILE%
