#!/usr/bin/env python3
"""Exercise the real hx terminal backend with a deterministic Unix PTY.

No terminal emulator, display server, or third-party Python packages are needed.
Run after a regular or optimized build: python3 contrib/terminal-smoke.py target/opt/hx
"""

import argparse
import base64
import codecs
from dataclasses import dataclass
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time
import unicodedata
import zlib


ROOT = Path(__file__).resolve().parent.parent
READ_ONLY = "Review mode is read-only; use :review-mode off to edit"
CSI = re.compile(r"\x1b\[([0-?]*)([ -/]*)([@-~])")


@dataclass
class Frame:
    width: int
    height: int
    row: int
    column: int
    cell_width: int
    cell_height: int
    alpha: int
    compressed: bool


class Screen:
    def __init__(self):
        self.rows, self.columns = 24, 80
        self.cell_width, self.cell_height = 10, 20
        self.x = self.y = 0
        self.foreground = self.background = None
        self.saved_cursor = (0, 0)
        self.native_cursor = True
        self.synchronized = False
        self.frames = []
        self.deletions = 0
        self.upload = None
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.clear()

    def clear(self):
        self.cells = [[(" ", None) for _ in range(self.columns)] for _ in range(self.rows)]

    def resize(self, rows, columns, cell_width, cell_height):
        self.rows, self.columns = rows, columns
        self.cell_width, self.cell_height = cell_width, cell_height
        self.x = self.y = 0
        self.clear()

    def lines(self):
        return ["".join(cell[0] for cell in row).rstrip() for row in self.cells]

    def text(self):
        return "\n".join(self.lines())

    def graphics(self, command):
        control, _, payload = command.partition(";")
        fields = dict(part.split("=", 1) for part in control.split(","))
        if fields.get("a") == "d":
            self.deletions += 1
            return
        if fields.get("a") == "T":
            assert self.upload is None, "interleaved graphics uploads"
            assert fields.get("f") == "32" and fields.get("t") == "d"
            self.upload = (fields, (self.y, self.x), bytearray())
        assert self.upload is not None, "graphics continuation without an upload"
        metadata, (row, column), content = self.upload
        content.extend(base64.b64decode(payload, validate=True))
        assert len(content) <= 4 * 1024 * 1024, "unbounded cursor image"
        if fields.get("m") == "1":
            return
        assert self.synchronized, "cursor image outside synchronized output"
        compressed = metadata.get("o") == "z"
        rgba = zlib.decompress(content) if compressed else content
        width, height = int(metadata["s"]), int(metadata["v"])
        assert len(rgba) == width * height * 4, "invalid RGBA frame length"
        offset_x, offset_y = int(metadata["X"]), int(metadata["Y"])
        assert 0 <= offset_x < self.cell_width and 0 <= offset_y < self.cell_height
        assert column * self.cell_width + offset_x + width <= self.columns * self.cell_width
        assert row * self.cell_height + offset_y + height <= self.rows * self.cell_height
        alpha = max(rgba[3::4])
        assert alpha > 0, "invisible graphics cursor"
        self.frames.append(Frame(width, height, row, column, self.cell_width,
                                 self.cell_height, alpha, compressed))
        self.upload = None

    def csi(self, arguments, intermediate, command, fd):
        private = arguments.startswith("?")
        values = [int(value or 0) for value in arguments.lstrip("?<>=").split(";")]
        first = values[0]
        if command in ("H", "f"):
            self.y = min(self.rows - 1, max(0, first - 1))
            column = values[1] if len(values) > 1 else 1
            self.x = min(self.columns - 1, max(0, column - 1))
        elif command == "G":
            self.x = min(self.columns - 1, max(0, first - 1))
        elif command == "d":
            self.y = min(self.rows - 1, max(0, first - 1))
        elif command in ("A", "B", "C", "D"):
            distance = first or 1
            if command == "A":
                self.y = max(0, self.y - distance)
            elif command == "B":
                self.y = min(self.rows - 1, self.y + distance)
            elif command == "C":
                self.x = min(self.columns - 1, self.x + distance)
            else:
                self.x = max(0, self.x - distance)
        elif command in ("J", "K"):
            for y in range(self.rows):
                for x in range(self.columns):
                    if command == "K" and y != self.y:
                        continue
                    position = (y, x) if command == "J" else x
                    cursor = (self.y, self.x) if command == "J" else self.x
                    if first in (2, 3) or first == 0 and position >= cursor or first == 1 and position <= cursor:
                        self.cells[y][x] = (" ", self.background)
        elif command == "m":
            index = 0
            while index < len(values):
                value = values[index]
                if value in (0, 49):
                    self.background = None
                if value in (38, 48) and index + 1 < len(values):
                    length = 5 if values[index + 1] == 2 else 3
                    if value == 48:
                        self.background = values[index + 2:index + length]
                    index += length - 1
                index += 1
        elif command in ("h", "l") and private:
            if first == 2026:
                self.synchronized = command == "h"
            elif first == 25:
                self.native_cursor = command == "h"
        elif command == "p" and intermediate == "$" and first == 2026:
            os.write(fd, b"\x1b[?2026;2$y")
        elif command == "n" and first == 6:
            os.write(fd, f"\x1b[{self.y + 1};{self.x + 1}R".encode())
        elif command == "c":
            os.write(fd, b"\x1b[?1;2c")
        elif command == "u" and private and arguments == "?":
            os.write(fd, b"\x1b[?0u")

    def feed(self, data, fd):
        self.pending += self.decoder.decode(data)
        index = 0
        while index < len(self.pending):
            character = self.pending[index]
            if character == "\x1b":
                if index + 1 == len(self.pending):
                    break
                kind = self.pending[index + 1]
                if kind == "[":
                    match = CSI.match(self.pending, index)
                    if match is None:
                        break
                    arguments, intermediate, command = match.groups()
                    # Colon-separated SGR is not used by the fixture theme.
                    if ":" not in arguments:
                        self.csi(arguments, intermediate, command, fd)
                    index = match.end()
                    continue
                if kind in ("]", "_", "P"):
                    end = self.pending.find("\x1b\\", index + 2)
                    bell = self.pending.find("\x07", index + 2) if kind == "]" else -1
                    if bell >= 0 and (end < 0 or bell < end):
                        end, delimiter = bell, 1
                    else:
                        delimiter = 2
                    if end < 0:
                        break
                    content = self.pending[index + 2:end]
                    if kind == "_" and content.startswith("G"):
                        self.graphics(content[1:])
                    elif kind == "]" and content in ("10;?", "11;?"):
                        color = "eeee/eeee/eeee" if content.startswith("10") else "2020/2020/2020"
                        os.write(fd, f"\x1b]{content[:2]};rgb:{color}\x1b\\".encode())
                    index = end + delimiter
                    continue
                if kind == "7":
                    self.saved_cursor = (self.x, self.y)
                elif kind == "8":
                    self.x, self.y = self.saved_cursor
                index += 2
                continue
            if character == "\r":
                self.x = 0
            elif character == "\n":
                self.y = min(self.rows - 1, self.y + 1)
            elif character == "\b":
                self.x = max(0, self.x - 1)
            elif character >= " ":
                if unicodedata.combining(character) and self.x:
                    old, background = self.cells[self.y][self.x - 1]
                    self.cells[self.y][self.x - 1] = (old + character, background)
                else:
                    self.cells[self.y][self.x] = (character, self.background)
                    width = 2 if unicodedata.east_asian_width(character) in ("W", "F") else 1
                    self.x = min(self.columns - 1, self.x + width)
            index += 1
        self.pending = self.pending[index:]


class Editor:
    def __init__(self, binary, fixture, term, program, multiplexed=False):
        self.fixture = fixture
        self.launch = binary, term, program, multiplexed
        self.source = fixture / "example.rs"
        base = "fn main() {\n    let old_name = 1;\n" + "".join(
            f'    let example_{index} = demo_value("some text");\n' for index in range(17)
        ) + "}\n"
        self.current = base.replace("old_name", "new_name")
        self.source.write_text(base)
        subprocess.run(["git", "init", "-q", "-b", "main", str(fixture)], check=True)
        subprocess.run(["git", "add", "example.rs"], cwd=fixture, check=True)
        subprocess.run(["git", "-c", "user.name=Terminal test", "-c",
                        "user.email=test@example.invalid", "-c", "commit.gpgsign=false",
                        "commit", "-qm", "baseline"], cwd=fixture, check=True)
        self.source.write_text(self.current)
        theme = fixture / "config/helix/themes/terminal-smoke.toml"
        theme.parent.mkdir(parents=True)
        theme.write_text('''"ui.background" = { bg = "#202020" }
"ui.text" = { fg = "#eeeeee" }
"ui.diff.added" = { bg = "#204020" }
"ui.diff.deleted" = { bg = "#402020" }
"ui.selection" = { bg = "#303030" }
"ui.cursor" = { bg = "#ffeeaa" }
"ui.review.comment" = { bg = "#202830" }
"ui.review.comment.active" = { bg = "#304050" }
"ui.review.reference" = { bg = "#404060" }
''')
        config = fixture / "config.toml"
        config.write_text('''theme = "terminal-smoke"
[editor]
scrolloff = 0
true-color = true
idle-timeout = 10000
kitty-keyboard-protocol = "disabled"
[editor.cursor-shape]
normal = "block"
insert = "bar"
select = "block"
[editor.lsp]
enable = false
[editor.cursor-smear]
enabled = true
duration = 120
max-distance = 40
''')
        self.start()

    def start(self):
        binary, term, program, multiplexed = self.launch
        fixture = self.fixture
        config = fixture / "config.toml"
        self.screen = Screen()
        self.raw = bytearray()
        self.status = None
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 800, 480))
            os.chdir(fixture)
            for name in ("TMUX", "STY", "ZELLIJ", "HELIX_DEFAULT_RUNTIME"):
                os.environ.pop(name, None)
            os.environ.update(TERM=term, TERM_PROGRAM=program, COLORTERM="truecolor",
                              HELIX_RUNTIME=str(ROOT / "runtime"),
                              XDG_CONFIG_HOME=str(fixture / "config"),
                              XDG_DATA_HOME=str(fixture / "data"),
                              XDG_CACHE_HOME=str(fixture / "cache"))
            if multiplexed:
                os.environ["TMUX"] = "/tmp/terminal-smoke,0,0"
            os.execv(str(binary), [str(binary), "--config", str(config),
                                  "--log", str(fixture / "helix.log"), self.source.name])
        self.set_size(24, 80, 10, 20)

    def reopen(self):
        self.close()
        self.start()
        self.wait_for(lambda: "fn main()" in self.screen.text(), "reopened document")

    def set_size(self, rows, columns, cell_width, cell_height):
        self.screen.resize(rows, columns, cell_width, cell_height)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ,
                    struct.pack("HHHH", rows, columns, columns * cell_width, rows * cell_height))

    def pump(self, seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if select.select([self.fd], [], [], min(0.025, max(0, deadline - time.monotonic())))[0]:
                try:
                    data = os.read(self.fd, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    break
                if not data:
                    break
                self.raw.extend(data)
                if len(self.raw) > 2 * 1024 * 1024:
                    del self.raw[:-2 * 1024 * 1024]
                self.screen.feed(data, self.fd)

    def wait_for(self, predicate, description, timeout=3):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.pump(0.025)
            if predicate():
                return
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.status = status
                raise AssertionError(f"hx exited while waiting for {description}: {status}")
        raise AssertionError(f"timed out waiting for {description}\n{self.screen.text()}")

    def keys(self, keys, settle=0.16):
        os.write(self.fd, keys.encode())
        self.pump(settle)

    def command(self, command):
        # Let the terminal distinguish Escape from an Alt-modified colon.
        self.keys("\x1b", settle=0.05)
        self.keys(":" + command + "\r")

    def close(self):
        try:
            if self.status is None:
                self.command("quit-all!")
                self.wait_for_exit()
            assert os.waitstatus_to_exitcode(self.status) == 0, self.status
        finally:
            self.abort()

    def abort(self):
        if self.status is None:
            os.kill(self.pid, signal.SIGKILL)
            _, self.status = os.waitpid(self.pid, 0)
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None

    def wait_for_exit(self):
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            self.pump(0.025)
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.status = status
                return
        raise AssertionError("hx did not exit")

    def check_review(self):
        self.keys("ggzr")
        assert "[readonly]" in self.screen.text(), self.screen.text()
        assert "old_name" in self.screen.text() and "new_name" in self.screen.text()
        for text, sign, background in [("old_name", "-", [64, 32, 32]),
                                       ("new_name", "+", [32, 64, 32])]:
            row = next(row for row in self.screen.cells if text in "".join(cell[0] for cell in row))
            assert row[0] == (sign, background) and row[-1][1] == background, row
        for keys in ("i", "d", "p", "u"):
            self.keys(keys)
            assert READ_ONLY in self.screen.text(), f"{keys}:\n{self.screen.text()}"
        for command in ("sort", "mv moved.rs", "encoding utf-16", "clipboard-paste-replace",
                        "lsp-workspace-command anything", "write!"):
            self.command(command)
            assert READ_ONLY in self.screen.text(), f"{command}:\n{self.screen.text()}"
        assert not (self.fixture / "moved.rs").exists()
        self.command("encoding")
        assert "utf-8" in self.screen.text().lower() and READ_ONLY not in self.screen.text()
        self.command("line-ending")
        assert READ_ONLY not in self.screen.text(), self.screen.text()
        self.keys("gg[D")
        assert self.screen.lines()[self.screen.y].startswith("-"), self.screen.text()
        self.keys("vx")
        self.keys("zr")
        assert "[readonly]" not in self.screen.text() and "old_name" not in self.screen.text()
        self.keys("\x1b")
        self.keys("vzr")
        assert "[readonly]" in self.screen.text(), self.screen.text()
        self.keys("zr")
        self.keys("\x1b")
        self.keys("Zr\x1b")
        assert "[readonly]" in self.screen.text(), self.screen.text()
        self.keys("Zr\x1b")
        assert "[readonly]" not in self.screen.text(), self.screen.text()
        self.keys("ggiEDITABLE \x1b")
        assert "EDITABLE" in self.screen.text(), self.screen.text()
        assert self.source.read_text() == self.current, "review changed the source file"

    def check_comments(self):
        sidecar = self.source.with_name(self.source.name + ".review.json")

        def comments():
            data = json.loads(sidecar.read_text())
            if data["version"] == 1:
                return data["comments"]
            return [dict(message, anchor=thread["anchor"]) for thread in data["threads"] for message in thread["messages"]]

        def row_containing(text):
            return next(row for row in self.screen.cells if text in "".join(cell[0] for cell in row))

        self.keys("ggzr c")
        assert " INS " in self.screen.text(), self.screen.text()
        self.keys("Line note\rSecond row\x1b")
        assert comments()[0]["anchor"]["quote"] == "fn main() {\n"
        assert comments()[0]["text"] == "Line note\nSecond row"
        row = row_containing("Line note")
        assert "".join(cell[0] for cell in row[:7]).strip() == "", "comment has a gutter label"
        assert row[0][1] == [48, 64, 80] and row[-1][1] == [48, 64, 80]
        assert self.screen.text().index("Line note") < self.screen.text().index("fn main()")
        self.reopen()
        self.keys("ggzr")
        self.wait_for(lambda: "Line note" in self.screen.text(), "saved review comment")
        self.keys("k")
        assert "Second row" in self.screen.lines()[self.screen.y], self.screen.text()
        assert row_containing("Line note")[0][1] == [48, 64, 80]
        self.keys("j")
        assert row_containing("Line note")[0][1] == [32, 40, 48], "comment remained active"

        self.command("goto 2")
        self.keys("8lv7l c")
        self.keys("Range note\x1b")
        assert comments()[1]["anchor"]["quote"] == "new_name", comments()
        row = row_containing("new_name")
        start = "".join(cell[0] for cell in row).index("new_name")
        assert all(cell[1] == [64, 64, 96] for cell in row[start:start + 8]), row
        assert row[start - 1][1] == [32, 64, 32], "character anchor tinted the whole line"
        self.command("goto 2")
        self.keys("8lv7l c")
        self.keys("Related note\x1b")
        assert row_containing("Range note")[0][1] == [48, 64, 80], "related comment was not highlighted"
        self.command("review-delete")
        assert len(comments()) == 2
        self.command("goto 2")
        self.keys("k")
        self.command("review-delete")
        assert len(comments()) == 1, comments()

        self.keys("gg[D8lv7l c")
        self.keys("Old note\x1b")
        assert comments()[1]["anchor"]["side"] == "base"
        assert comments()[1]["anchor"]["quote"] == "old_name", comments()
        assert self.screen.text().index("Old note") < self.screen.text().index("old_name")
        self.keys("A edited\x1b")
        assert comments()[1]["text"] == "Old note edited", comments()
        self.keys("%d")
        assert comments()[1]["text"] == "", "normal-mode delete did not edit the comment"
        self.keys("u")
        assert comments()[1]["text"] == "Old note edited", "comment undo changed its source"
        self.keys("i unsaved\x03")
        assert comments()[1]["text"] == "Old note edited", "cancel changed the saved comment"
        self.command("goto 2")
        self.keys("kk")
        self.command("review-delete")
        assert len(comments()) == 1, comments()
        self.keys("ggk")
        self.command("review-delete")
        assert not sidecar.exists(), "removing the final comment retained the sidecar"
        self.keys("zr")
        assert self.source.read_text() == self.current, "comments changed the source file"


def graphics_smoke(editor, compressed):
    screen = editor.screen
    editor.wait_for(lambda: bool(screen.frames), "initial cursor image")
    editor.pump(0.25)
    before = len(screen.frames)
    editor.keys("7j", settle=0.25)
    frames = screen.frames[before:]
    assert len(frames) >= 3, "cursor jump produced no animation frames"
    assert any(frame.height > frame.cell_height for frame in frames), "no vertical smear"
    assert all(frame.compressed == compressed for frame in screen.frames)
    editor.keys("28l", settle=0.25)
    assert any(frame.width > frame.cell_width for frame in screen.frames), "no horizontal smear"
    editor.keys("gg", settle=0.25)
    assert any(frame.width > frame.cell_width and frame.height > frame.cell_height
               for frame in screen.frames), "no diagonal smear"
    settled = len(screen.frames)
    editor.pump(0.18)
    # Background document work can repaint a static cursor after it settles.
    assert all(frame.alpha == 255 and (frame.width, frame.height) ==
               (frame.cell_width, frame.cell_height) for frame in screen.frames[settled:]), \
        "animation continued after the cursor settled"
    assert screen.frames[-1].alpha == 255 and not screen.native_cursor
    deleted = screen.deletions
    editor.keys("\x1b[O")
    assert screen.deletions > deleted, "focus loss retained the graphics cursor"
    lost = len(screen.frames)
    editor.pump(0.18)
    assert len(screen.frames) == lost, "animation continued without terminal focus"
    editor.keys("\x1b[I")
    assert len(screen.frames) > lost, "focus gain did not restore the cursor"
    editor.set_size(22, 76, 12, 24)
    editor.wait_for(lambda: (screen.frames[-1].width, screen.frames[-1].height) == (12, 24),
                    "resized cursor metrics")
    editor.pump(0.16)
    assert (screen.frames[-1].width, screen.frames[-1].height) == (12, 24)
    editor.set_size(24, 80, 10, 20)
    editor.wait_for(lambda: (screen.frames[-1].width, screen.frames[-1].height) == (10, 20),
                    "restored cursor metrics")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    assert binary.is_file(), f"binary not found: {binary}"
    for name, term, program, multiplexed, graphics in [
        ("kitty", "xterm-kitty", "kitty", False, True),
        ("ghostty", "xterm-ghostty", "ghostty", False, True),
        ("ordinary", "xterm-256color", "terminal-smoke", False, False),
        ("multiplexed", "xterm-kitty", "kitty", True, False),
    ]:
        with tempfile.TemporaryDirectory(prefix=f"helix-terminal-{name}-") as directory:
            editor = Editor(binary, Path(directory), term, program, multiplexed)
            try:
                editor.wait_for(lambda: "new_name" in editor.screen.text(), "document rendering")
                if graphics:
                    graphics_smoke(editor, compressed=name == "kitty")
                else:
                    editor.keys("7j28lgg")
                    assert not editor.screen.frames, "unsupported session emitted graphics"
                    assert any(cell[1] == [255, 238, 170] for row in editor.screen.cells
                               for cell in row), "software block cursor was missing"
                    editor.keys("i")
                    assert editor.screen.native_cursor, "insert cursor was hidden"
                    editor.keys("\x1b")
                    assert not editor.screen.frames, "insert mode emitted unsupported graphics"
                editor.check_comments()
                if graphics:
                    editor.check_review()
                editor.close()
                print(f"{name}: passed", flush=True)
            except BaseException:
                print(editor.screen.text(), flush=True)
                log = editor.fixture / "helix.log"
                if log.exists():
                    print(log.read_text()[-8000:], flush=True)
                editor.abort()
                raise


if __name__ == "__main__":
    main()
