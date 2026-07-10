#!/usr/bin/env python3
"""
naut

Never Lose A Thought

A TUI autosave daemon that continuously snapshots your working tree
onto a dedicated branch, with history, diffs, and one-key restore.
"""

from __future__ import annotations

import subprocess
import threading
from datetime import datetime
from pathlib import Path

from rich.text import Text
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Button, DataTable, Footer, Header, Label, RichLog, Static

from watchdog.events import FileSystemEventHandler
from watchdog.observers import Observer

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

AUTOSAVE_BRANCH = "autosave"
DEBOUNCE_SECONDS = 0.5
MAX_HISTORY = 50
IGNORED_DIRS = {".git", "__pycache__", "node_modules", ".venv", "venv", ".tox", ".mypy_cache"}

# ---------------------------------------------------------------------------
# Git helpers
# ---------------------------------------------------------------------------


def _git(*args: str, cwd: Path | None = None, capture: bool = False) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", *args],
        cwd=str(cwd) if cwd else None,
        text=True,
        capture_output=capture,
    )


class NautGit:
    """Thin wrapper around git for naut operations."""

    def __init__(self, root: Path) -> None:
        self.root = root

    # -- repo checks -------------------------------------------------------

    def is_repo(self) -> bool:
        return _git("rev-parse", cwd=self.root, capture=True).returncode == 0

    def current_branch(self) -> str:
        r = _git("branch", "--show-current", cwd=self.root, capture=True)
        return r.stdout.strip() or "HEAD detached"

    def short_head(self) -> str:
        r = _git("rev-parse", "--short", "HEAD", cwd=self.root, capture=True)
        return r.stdout.strip() or "???"

    # -- autosave branch ---------------------------------------------------

    def ensure_autosave_branch(self) -> None:
        r = _git("show-ref", "--verify", f"refs/heads/{AUTOSAVE_BRANCH}", cwd=self.root, capture=True)
        if r.returncode != 0:
            _git("branch", AUTOSAVE_BRANCH, cwd=self.root)

    # -- snapshot ----------------------------------------------------------

    def snapshot(self) -> bool:
        """Create an autosave snapshot. Returns True if a commit was made."""
        # Save current HEAD so we can return to it
        head_r = _git("rev-parse", "HEAD", cwd=self.root, capture=True)
        if head_r.returncode != 0:
            return False
        original_head = head_r.stdout.strip()

        # Stash everything (including untracked files)
        _git("stash", "push", "-u", "-m", "naut-snapshot", cwd=self.root)

        # Check if anything was stashed
        stash_list = _git("stash", "list", cwd=self.root, capture=True)
        if f"naut-snapshot" not in stash_list.stdout:
            # Nothing to save
            return False

        # Create a temporary branch at the stash, commit onto it
        _git("stash", "branch", "_naut_tmp", cwd=self.root)

        # Move autosave branch to point at this commit
        _git("branch", "-f", AUTOSAVE_BRANCH, "_naut_tmp", cwd=self.root)

        # Switch back to original branch
        branch_name = self.current_branch()
        if branch_name == "_naut_tmp":
            _git("checkout", original_head, cwd=self.root)
        else:
            _git("checkout", branch_name, cwd=self.root)

        # Delete the temporary branch
        _git("branch", "-D", "_naut_tmp", cwd=self.root)

        return True

    # -- history -----------------------------------------------------------

    def history(self, count: int = MAX_HISTORY) -> list[dict]:
        """Return recent autosave commits as list of dicts."""
        r = _git(
            "log",
            AUTOSAVE_BRANCH,
            f"-{count}",
            "--format=%H|%h|%s|%ai",
            cwd=self.root,
            capture=True,
        )
        if r.returncode != 0:
            return []

        entries = []
        for line in r.stdout.strip().splitlines():
            if not line:
                continue
            parts = line.split("|", 3)
            if len(parts) == 4:
                full, short, subject, date = parts
                entries.append({
                    "hash": full.strip(),
                    "short": short.strip(),
                    "subject": subject.strip(),
                    "date": date.strip(),
                })
        return entries

    # -- diff --------------------------------------------------------------

    def diff(self, commit_hash: str) -> str:
        """Return unified diff for a given autosave commit."""
        r = _git(
            "diff",
            f"{commit_hash}~1..{commit_hash}",
            cwd=self.root,
            capture=True,
        )
        return r.stdout if r.returncode == 0 else ""

    # -- restore -----------------------------------------------------------

    def restore(self, commit_hash: str) -> tuple[bool, str]:
        """Restore working tree to a given autosave commit.

        Returns (success, message).
        """
        # Stash any uncommitted changes
        _git("stash", "push", "-u", "-m", "naut-pre-restore", cwd=self.root)

        # Hard reset to the snapshot
        r = _git("reset", "--hard", commit_hash, cwd=self.root)
        if r.returncode != 0:
            _git("stash", "pop", cwd=self.root)
            return False, "Failed to reset to snapshot."

        # Try to reapply stashed changes
        pop = _git("stash", "pop", cwd=self.root)
        if pop.returncode != 0:
            return True, f"Restored to {commit_hash[:7]}. Stash conflicts left in stash list."

        return True, f"Restored to {commit_hash[:7]}."


# ---------------------------------------------------------------------------
# File watcher
# ---------------------------------------------------------------------------


class _ChangeHandler(FileSystemEventHandler):
    """Debounced file change handler that triggers snapshots."""

    def __init__(self, app: NautApp) -> None:  # type: ignore[name-defined]
        super().__init__()
        self._app = app
        self._timer: threading.Timer | None = None
        self._lock = threading.Lock()

    def on_any_event(self, event) -> None:  # noqa: ANN001
        if event.is_directory:
            return

        src = getattr(event, "src_path", "")
        # Skip ignored directories
        for ignored in IGNORED_DIRS:
            if ignored in src:
                return

        with self._lock:
            if self._timer is not None:
                self._timer.cancel()
            self._timer = threading.Timer(DEBOUNCE_SECONDS, self._fire)
            self._timer.start()

    def _fire(self) -> None:
        try:
            self._app.request_snapshot()
        except Exception:
            pass


# ---------------------------------------------------------------------------
# Confirm restore modal
# ---------------------------------------------------------------------------


class ConfirmRestore(ModalScreen[bool]):
    """Asks the user to confirm a restore."""

    CSS = """
    ConfirmRestore {
        align: center middle;
    }
    #dialog {
        width: 50;
        height: auto;
        background: $surface;
        border: thick $primary;
        padding: 1 2;
    }
    #question {
        height: auto;
        text-align: center;
        margin-bottom: 1;
    }
    Horizontal {
        height: auto;
        align: center middle;
    }
    Button {
        margin: 0 1;
        min-width: 12;
    }
    """

    def __init__(self, short_hash: str, branch: str) -> None:
        super().__init__()
        self._label = f"Restore to [bold]{short_hash}[/bold]?\nWorking tree will be reset. Current changes stashed."

    def compose(self) -> ComposeResult:
        with Vertical(id="dialog"):
            yield Label(self._label, id="question")
            with Horizontal():
                yield Button("Restore", variant="error", id="confirm")
                yield Button("Cancel", variant="primary", id="cancel")

    def on_button_pressed(self, event: Button.Pressed) -> None:
        self.dismiss(event.button.id == "confirm")


# ---------------------------------------------------------------------------
# Main app
# ---------------------------------------------------------------------------


class NautApp(App):
    """naut — Never Lose A Thought TUI."""

    TITLE = "naut"
    SUB_TITLE = "never lose a thought"

    CSS = """
    Screen {
        layout: vertical;
    }
    Header { dock: top; }
    Footer { dock: bottom; }

    #status-bar {
        height: 3;
        background: $surface;
        padding: 0 2;
        border-bottom: solid $primary;
    }

    #main-area {
        height: 1fr;
        layout: horizontal;
    }

    #snapshot-panel {
        width: 2fr;
        height: 100%;
        border-right: solid $primary;
    }

    #diff-panel {
        width: 3fr;
        height: 100%;
    }

    DataTable > .datatable--header {
        text-style: bold;
        background: $primary;
    }
    """

    BINDINGS = [
        Binding("s", "snapshot", "Snapshot"),
        Binding("r", "restore", "Restore"),
        Binding("d", "toggle_diff", "Diff"),
        Binding("q", "quit", "Quit"),
    ]

    def __init__(self, root: Path) -> None:
        super().__init__()
        self.root = root
        self.git = NautGit(root)
        self._watcher: Observer | None = None
        self._diff_visible = True
        self._last_snapshot_time: str | None = None

    # -- compose -----------------------------------------------------------

    def compose(self) -> ComposeResult:
        yield Header()

        with Horizontal(id="main-area"):
            with Vertical(id="snapshot-panel"):
                yield Static(self._status_text(), id="status-bar")
                yield DataTable(id="snapshots")

            with VerticalScroll(id="diff-panel"):
                yield RichLog(
                    highlight=True,
                    markup=True,
                    wrap=False,
                    auto_scroll=False,
                    id="diff-log",
                )

        yield Footer()

    # -- lifecycle ---------------------------------------------------------

    def on_mount(self) -> None:
        # Init snapshot table
        table = self.query_one("#snapshots", DataTable)
        table.add_columns("Hash", "When", "Branch")
        table.zebra_stripes = True
        table.cursor_type = "row"

        # Ensure autosave branch exists
        self.git.ensure_autosave_branch()

        # Load history
        self._refresh_history()

        # Start file watcher
        self._start_watcher()

        # Periodic status refresh
        self.set_interval(2.0, self._tick)

    def on_unmount(self) -> None:
        if self._watcher is not None:
            self._watcher.stop()
            self._watcher.join(timeout=2)

    # -- watcher -----------------------------------------------------------

    def _start_watcher(self) -> None:
        handler = _ChangeHandler(self)
        self._watcher = Observer()
        self._watcher.schedule(handler, str(self.root), recursive=True)
        self._watcher.daemon = True
        self._watcher.start()

    # -- snapshot ----------------------------------------------------------

    def request_snapshot(self) -> None:
        """Called from watchdog thread."""
        self.call_from_thread(self._do_snapshot)

    def _do_snapshot(self) -> None:
        try:
            made = self.git.snapshot()
        except Exception as exc:
            self.notify(f"Snapshot error: {exc}", severity="error")
            return

        if made:
            now = datetime.now().strftime("%H:%M:%S")
            self._last_snapshot_time = now
            self.notify(f"Snapshot saved at {now}")
            self._refresh_history()
            self._refresh_diff_for_selected()
            self._refresh_status()

    def action_snapshot(self) -> None:
        self._do_snapshot()

    # -- history -----------------------------------------------------------

    def _refresh_history(self) -> None:
        table = self.query_one("#snapshots", DataTable)
        table.clear()

        entries = self.git.history()
        for e in entries:
            # Parse the date for display
            try:
                dt = datetime.fromisoformat(e["date"])
                when = dt.strftime("%b %d %H:%M")
            except ValueError:
                when = e["date"][:16]

            table.add_row(e["short"], when, AUTOSAVE_BRANCH, key=e["hash"])

        self._refresh_status()

    # -- diff --------------------------------------------------------------

    def _toggle_diff_panel(self, show: bool) -> None:
        panel = self.query_one("#diff-panel")
        panel.styles.display = "block" if show else "none"

    def action_toggle_diff(self) -> None:
        self._diff_visible = not self._diff_visible
        self._toggle_diff_panel(self._diff_visible)

    def _refresh_diff_for_selected(self) -> None:
        if not self._diff_visible:
            return

        table = self.query_one("#snapshots", DataTable)
        log = self.query_one("#diff-log", RichLog)

        if not table.rows:
            log.clear()
            log.write("[dim]No snapshots yet.[/dim]")
            return

        try:
            row_key = table.get_row_at(table.cursor_row)[0]
            # The key we stored is the full hash
            selected_key = table.rows[table.cursor_row]
        except Exception:
            return

        full_hash = str(selected_key)
        diff_text = self.git.diff(full_hash)

        log.clear()

        if not diff_text.strip():
            log.write("[dim]No diff available for this snapshot.[/dim]")
            return

        for line in diff_text.splitlines():
            if line.startswith("diff "):
                log.write(Text(line, style="bold cyan"))
            elif line.startswith("index "):
                log.write(Text(line, style="dim"))
            elif line.startswith("---") or line.startswith("+++"):
                log.write(Text(line, style="bold"))
            elif line.startswith("@@"):
                log.write(Text(line, style="bold cyan"))
            elif line.startswith("+"):
                log.write(Text(line, style="bold green"))
            elif line.startswith("-"):
                log.write(Text(line, style="bold red"))
            else:
                log.write(line)

    def on_data_table_row_selected(self, event: DataTable.RowSelected) -> None:
        self._refresh_diff_for_selected()

    # -- restore -----------------------------------------------------------

    def action_restore(self) -> None:
        table = self.query_one("#snapshots", DataTable)

        if not table.rows:
            self.notify("No snapshots to restore.", severity="warning")
            return

        try:
            row_key = table.rows[table.cursor_row]
        except Exception:
            return

        full_hash = str(row_key)
        short_hash = full_hash[:7]

        def on_confirm(confirmed: bool | None) -> None:
            if not confirmed:
                return
            branch = self.git.current_branch()
            ok, msg = self.git.restore(full_hash)
            if ok:
                self.notify(msg)
                self._refresh_history()
                self._refresh_diff_for_selected()
            else:
                self.notify(msg, severity="error")

        self.push_screen(ConfirmRestore(short_hash, self.git.current_branch()), on_confirm)

    # -- status ------------------------------------------------------------

    def _status_text(self) -> str:
        branch = self.git.current_branch()
        count = len(self.git.history())
        last = self._last_snapshot_time or "never"
        return f"  Branch: [bold]{branch}[/bold]    Autosave: [bold]{AUTOSAVE_BRANCH}[/bold]    Snapshots: {count}    Last: {last}"

    def _refresh_status(self) -> None:
        try:
            bar = self.query_one("#status-bar", Static)
            bar.update(self._status_text())
        except Exception:
            pass

    # -- tick ---------------------------------------------------------------

    def _tick(self) -> None:
        self._refresh_status()


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def main() -> None:
    root = Path.cwd()

    git = NautGit(root)
    if not git.is_repo():
        print("Not inside a git repository.")
        raise SystemExit(1)

    app = NautApp(root)
    app.run()


if __name__ == "__main__":
    main()
