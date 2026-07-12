#!/usr/bin/env python3
"""BedrockOS - Build and Run TUI."""

import os
import shutil
import subprocess
import sys

from textual import on, work
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical
from textual.widgets import (
    Button,
    Footer,
    Header,
    Label,
    LoadingIndicator,
    RadioSet,
    RadioButton,
    RichLog,
    Static,
)

WORKSPACE = os.path.dirname(os.path.abspath(__file__))
TARGET_DIR = os.path.join(WORKSPACE, "target")
QEMU_DIR = r"C:\Program Files\qemu"
QEMU_PATH = os.path.join(QEMU_DIR, "qemu-system-x86_64.exe")
OVMF_CODE_SRC = os.path.join(QEMU_DIR, "share", "edk2-x86_64-code.fd")
# This QEMU build ships no edk2-x86_64-vars.fd; the varstore FV is
# architecture-independent and edk2-i386-vars.fd is the same empty store at the
# correct size. Prefer x86_64 vars if present, else fall back to i386 vars.
OVMF_VARS_SRC = os.path.join(QEMU_DIR, "share", "edk2-x86_64-vars.fd")
if not os.path.exists(OVMF_VARS_SRC):
    OVMF_VARS_SRC = os.path.join(QEMU_DIR, "share", "edk2-i386-vars.fd")


class BuildApp(App):
    """TUI for building and running BedrockOS."""

    CSS = """
    Screen {
        layout: vertical;
    }
    #main {
        height: 1fr;
    }
    #controls {
        height: auto;
        padding: 1;
    }
    #log {
        height: 1fr;
        border: solid green;
    }
    RadioSet {
        width: 1fr;
    }
    Horizontal {
        height: auto;
    }
    Button {
        width: 1fr;
        margin: 0 1;
    }
    Label {
        width: 1fr;
    }
    """

    BINDINGS = [
        Binding("q", "quit", "Quit"),
        Binding("b", "build", "Build", priority=True),
        Binding("r", "run_qemu", "Run QEMU", priority=True),
        Binding("f", "full", "Full Run", priority=True),
    ]

    def compose(self) -> ComposeResult:
        yield Header()
        with Vertical(id="main"):
            with Horizontal(id="controls"):
                with Vertical():
                    yield Label("Target Package:")
                    yield RadioSet(
                        RadioButton("boot (UEFI app)", value=True),
                        RadioButton("kernel (bare metal)"),
                        RadioButton("All"),
                        id="package_select",
                    )
                with Vertical():
                    yield Label("Profile:")
                    yield RadioSet(
                        RadioButton("debug", value=True),
                        RadioButton("release"),
                        id="profile_select",
                    )
            yield RichLog(id="log", highlight=True, markup=True)
            with Horizontal(id="buttons"):
                yield Button("Build [B]", id="btn_build", variant="primary")
                yield Button("Run QEMU [R]", id="btn_run", variant="success")
                yield Button("Full Run [F]", id="btn_full", variant="warning")
                yield Button("Quit [Q]", id="btn_quit", variant="error")
        yield Footer()

    def action_build(self):
        self.run_build()

    def action_run_qemu(self):
        self.run_qemu()

    def action_full(self):
        self.run_full()

    @on(Button.Pressed, "#btn_build")
    def handle_build(self, event: Button.Pressed):
        self.run_build()

    @on(Button.Pressed, "#btn_run")
    def handle_run(self, event: Button.Pressed):
        self.run_qemu()

    @on(Button.Pressed, "#btn_full")
    def handle_full(self, event: Button.Pressed):
        self.run_full()

    @on(Button.Pressed, "#btn_quit")
    def handle_quit(self, event: Button.Pressed):
        self.exit()

    def get_selected_package(self) -> str:
        radio_set = self.query_one("#package_select", RadioSet)
        if radio_set.pressed_index == 0:
            return "boot"
        elif radio_set.pressed_index == 1:
            return "kernel"
        else:
            return "all"

    def get_selected_profile(self) -> str:
        radio_set = self.query_one("#profile_select", RadioSet)
        return "release" if radio_set.pressed_index == 1 else "dev"

    def write_log(self, message: str):
        log_widget = self.query_one("#log", RichLog)
        log_widget.write(message)

    @work(thread=True)
    def run_build(self):
        self._do_build()

    def _do_build(self):
        package = self.get_selected_package()
        profile = self.get_selected_profile()
        self.write_log(f"[bold blue]Building {package} ({profile})...[/]")

        if package in ("boot", "all"):
            self.build_target("x86_64-unknown-uefi", profile, "boot")

        if package in ("kernel", "all"):
            self.build_target("x86_64-unknown-none", profile, "kernel")

        self.write_log("[bold green]Build complete![/]")

    def build_target(self, target: str, profile: str, pkg: str):
        cmd = [
            "cargo", "build",
            "--target", target,
            "--profile", profile,
            "-p", pkg,
        ]
        self.write_log(f"  $ {' '.join(cmd)}")
        try:
            result = subprocess.run(
                cmd,
                cwd=WORKSPACE,
                capture_output=True,
                text=True,
                timeout=300,
            )
            if result.stdout:
                for line in result.stdout.strip().split("\n"):
                    self.write_log(f"  {line}")
            if result.returncode != 0:
                self.write_log(f"[bold red]Build failed with exit code {result.returncode}[/]")
                if result.stderr:
                    for line in result.stderr.strip().split("\n"):
                        self.write_log(f"  [red]{line}[/]")
            else:
                self.write_log(f"[green]  {pkg} built successfully[/]")
        except subprocess.TimeoutExpired:
            self.write_log("[red]Build timed out[/]")
        except Exception as e:
            self.write_log(f"[red]Build error: {e}[/]")

    @work(thread=True)
    def run_qemu(self):
        self._do_qemu()

    def _do_qemu(self):
        self.write_log("[bold blue]Creating disk image...[/]")
        self.create_disk_image()

        self.write_log("[bold blue]Running QEMU...[/]")
        img_path = os.path.join(TARGET_DIR, "os.img")
        if not os.path.exists(img_path):
            self.write_log(f"[red]Disk image not found: {img_path}[/]")
            return

        if not os.path.exists(OVMF_CODE_SRC):
            self.write_log(f"[red]OVMF code not found: {OVMF_CODE_SRC}[/]")
            return
        if not os.path.exists(OVMF_VARS_SRC):
            self.write_log(f"[red]OVMF vars template not found: {OVMF_VARS_SRC}[/]")
            return

        ovmf_code = os.path.join(TARGET_DIR, "ovmf_code.fd")
        ovmf_vars = os.path.join(TARGET_DIR, "ovmf_vars.fd")
        shutil.copyfile(OVMF_CODE_SRC, ovmf_code)
        shutil.copyfile(OVMF_VARS_SRC, ovmf_vars)

        qemu_cmd = [
            QEMU_PATH,
            "-drive", f"if=pflash,format=raw,readonly=on,file={ovmf_code}",
            "-drive", f"if=pflash,format=raw,file={ovmf_vars}",
            "-drive", f"format=raw,file={img_path}",
            "-m", "256M",
            "-nographic",
            "-serial", "mon:stdio",
        ]
        self.write_log(f"  $ {' '.join(qemu_cmd)}")
        try:
            result = subprocess.run(
                qemu_cmd,
                cwd=WORKSPACE,
                capture_output=True,
                text=True,
            )
            if result.stdout:
                for line in result.stdout.strip().split("\n"):
                    self.write_log(f"  {line}")
            if result.returncode != 0:
                self.write_log(f"[yellow]QEMU exited with code {result.returncode}[/]")
        except Exception as e:
            self.write_log(f"[red]QEMU error: {e}[/]")

    def create_disk_image(self):
        self.write_log("[bold blue]Creating GPT disk image via build_image.py...[/]")
        import build_image
        boot = os.path.join(TARGET_DIR, "x86_64-unknown-uefi", "debug", "boot.efi")
        if not os.path.exists(boot):
            boot = os.path.join(TARGET_DIR, "x86_64-unknown-uefi", "release", "boot.efi")
        kernel = os.path.join(TARGET_DIR, "x86_64-unknown-none", "debug", "kernel")
        if not os.path.exists(kernel):
            kernel = os.path.join(TARGET_DIR, "x86_64-unknown-none", "release", "kernel")
        if not os.path.exists(boot) or not os.path.exists(kernel):
            self.write_log("[red]Boot or kernel binary not found — build first[/]")
            return
        try:
            build_image.create_gpt_image(boot, kernel)
            self.write_log("[green]Disk image created[/]")
        except Exception as e:
            self.write_log(f"[red]Disk image creation error: {e}[/]")

    @work(thread=True)
    def run_full(self):
        # Run sequentially in THIS worker thread. Calling the @work-decorated
        # run_build()/run_qemu() would schedule two separate workers that race,
        # launching QEMU before the build finished.
        self._do_build()
        self._do_qemu()


if __name__ == "__main__":
    app = BuildApp()
    app.run()
