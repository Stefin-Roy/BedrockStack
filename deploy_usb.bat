@echo off
setlocal EnableDelayedExpansion
set SCRIPT_DIR=%~dp0
set TARGET_DIR=%SCRIPT_DIR%target
set USB_DIR=%SCRIPT_DIR%USB

REM Build kernel with display_log + cpu_slow (release)
echo [1/3] Building kernel (release, features: display_log cpu_slow)...
cargo build --target x86_64-unknown-none -p kernel --features "display_log cpu_slow" --release
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
