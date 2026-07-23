@echo off
setlocal EnableDelayedExpansion
set SCRIPT_DIR=%~dp0
set TARGET_DIR=%SCRIPT_DIR%target
set USB_DIR=%SCRIPT_DIR%USB

set BOOT_MODE=%1
if "%BOOT_MODE%"=="" set BOOT_MODE=uefi

if /i "%BOOT_MODE%"=="grub" goto :deploy_grub
goto :deploy_uefi

:deploy_uefi
REM Build kernel with display_log + cpu_slow (release)
echo [1/3] Building kernel (release, features: display_log cpu_slow)...
cargo build --target x86_64-unknown-none -p kernel --features "display_log" --release
if %errorlevel% neq 0 exit /b %errorlevel%

REM Build bootloader with cpu_slow (release)
echo [2/3] Building bootloader (release, features: cpu_slow)...
cargo build --target x86_64-unknown-uefi -p boot --release
if %errorlevel% neq 0 exit /b %errorlevel%

REM Copy to USB folder
echo [3/3] Copying to USB folder...
rmdir /S /Q "%USB_DIR%" 2>nul
mkdir "%USB_DIR%\EFI\BOOT"
mkdir "%USB_DIR%\EFI\BEDROCK"
copy /Y "%TARGET_DIR%\x86_64-unknown-uefi\release\boot.efi" "%USB_DIR%\EFI\BOOT\BOOTX64.EFI"
copy /Y "%TARGET_DIR%\x86_64-unknown-none\release\kernel" "%USB_DIR%\EFI\BEDROCK\KERNEL"
echo Done. Copy contents of USB\ to your ESP.
goto :done

:deploy_grub
echo === GRUB+Multiboot2 USB deployment ===
echo [1/2] Building kernel (release, kernelmb2)...
cargo build --target x86_64-unknown-none -p kernel --features "display_log kernelmb2" --release
if %errorlevel% neq 0 exit /b %errorlevel%

echo [2/2] Creating GRUB standalone image and copying to USB folder...
set GRUB_CFG=%TARGET_DIR%\grub.cfg
set GRUB_EFI=%TARGET_DIR%\grub_bootx64.efi

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

REM Convert TARGET_DIR to WSL path
set "WSL_TARGET=%TARGET_DIR:\=/%"
set DRIVE_LETTER=%WSL_TARGET:~0,1%
for %%a in (a b c d e f g h i j k l m n o p q r s t u v w x y z) do if /i "%%a"=="%DRIVE_LETTER%" set DRIVE_LETTER=%%a
set "WSL_TARGET=/mnt/%DRIVE_LETTER%%WSL_TARGET:~2%"

REM Run grub-mkstandalone via WSL
wsl bash -c "set -euo pipefail; if ! command -v grub-mkstandalone >/dev/null 2>&1; then sudo apt-get update -qq && sudo apt-get install -y -qq grub-efi-amd64-bin; fi; grub-mkstandalone -O x86_64-efi -o '%WSL_TARGET%/grub_bootx64.efi' --modules='part_gpt fat multiboot2' 'boot/grub/grub.cfg=%WSL_TARGET%/grub.cfg'"
if %errorlevel% neq 0 exit /b %errorlevel%

REM Copy to USB folder
rmdir /S /Q "%USB_DIR%" 2>nul
mkdir "%USB_DIR%\EFI\BOOT"
mkdir "%USB_DIR%\EFI\BEDROCK"
copy /Y "%GRUB_EFI%" "%USB_DIR%\EFI\BOOT\BOOTX64.EFI"
copy /Y "%TARGET_DIR%\x86_64-unknown-none\release\kernel" "%USB_DIR%\EFI\BEDROCK\KERNEL"
echo Done. Copy contents of USB\ to your ESP (boot via UEFI GRUB which loads kernel via multiboot2).
goto :done

:done
