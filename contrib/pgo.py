#!/usr/bin/env python3
"""Build, train and install a native Helix binary with profile-guided optimization."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import shlex
import struct
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[1]
ARTIFACTS = ROOT / "target" / "pgo"
RAW = ARTIFACTS / "raw"
MERGED = ARTIFACTS / "helix.profdata"
METADATA = ARTIFACTS / "build.json"
INSTRUMENTED = ARTIFACTS / "instrumented"
OPTIMIZED = ARTIFACTS / "optimized"


def output(*command):
    return subprocess.check_output(command, cwd=ROOT, text=True).strip()


def compiler():
    version = output("rustc", "-vV")
    host = next(line.split(": ", 1)[1] for line in version.splitlines() if line.startswith("host: "))
    return {"compiler": version, "host": host}


def source_hash():
    paths = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z",
         "--", "*.rs", "Cargo.toml", "**/Cargo.toml", "Cargo.lock"], cwd=ROOT
    ).split(b"\0")
    digest = hashlib.sha256()
    for path in sorted(set(paths) - {b""}):
        digest.update(path + b"\0")
        digest.update((ROOT / os.fsdecode(path)).read_bytes())
    return digest.hexdigest()


def llvm_profdata(info):
    suffix = ".exe" if os.name == "nt" else ""
    tool = Path(output("rustc", "--print", "sysroot")) / "lib" / "rustlib" / info["host"] / "bin" / ("llvm-profdata" + suffix)
    if not tool.is_file():
        raise RuntimeError("Install the matching profiling tools first: rustup component add llvm-tools-preview")
    return tool


def inherited_flags():
    if "CARGO_ENCODED_RUSTFLAGS" in os.environ:
        return os.environ["CARGO_ENCODED_RUSTFLAGS"].split("\x1f")
    return shlex.split(os.environ.get("RUSTFLAGS", ""))


def cargo_flags(kind):
    return ["-C", "target-cpu=native", "-C", f"profile-{kind}={RAW if kind == 'generate' else MERGED}"]


def cargo(command, target_dir, info, kind, install_root=None):
    flags = cargo_flags(kind)
    env = os.environ.copy()
    # Normal target configuration merges with .cargo/config.toml. Explicit
    # environment flags override it, so retain them and restore required flags.
    inherited = inherited_flags()
    if "RUSTFLAGS" in env or "CARGO_ENCODED_RUSTFLAGS" in env:
        env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(
            inherited + ["--cfg", "tokio_unstable", "-C", "target-feature=-crt-static"] + flags
        )
    config = 'target."cfg(all())".rustflags=' + json.dumps(flags)
    arguments = ["cargo", *command, "--profile", "opt", "--locked",
                 "--target", info["host"], "--target-dir", str(target_dir),
                 "--config", config, "--jobs", os.environ.get("CARGO_BUILD_JOBS", str(min(4, os.cpu_count() or 1)))]
    if install_root:
        arguments += ["--root", install_root]
    print(shlex.join(arguments), flush=True)
    subprocess.run(arguments, cwd=ROOT, env=env, check=True)


def binary(info):
    return INSTRUMENTED / info["host"] / "opt" / ("hx.exe" if os.name == "nt" else "hx")


def build():
    info = compiler()
    llvm_profdata(info)
    source = source_hash()
    RAW.mkdir(parents=True, exist_ok=True)
    for profile in RAW.glob("helix-*.profraw"):
        profile.unlink()
    MERGED.unlink(missing_ok=True)
    METADATA.unlink(missing_ok=True)
    cargo(["build", "--package", "helix-term", "--bin", "hx"], INSTRUMENTED, info, "generate")
    if source != source_hash():
        raise RuntimeError("Source changed during instrumentation; run just pgo-build again")
    info.update(source=source, flags=inherited_flags(), binary=hashlib.sha256(binary(info).read_bytes()).hexdigest())
    METADATA.write_text(json.dumps(info, indent=2) + "\n")
    print(f"Instrumented binary: {binary(info)}", flush=True)


def checked_build():
    if not METADATA.is_file():
        raise RuntimeError("Run just pgo-build first")
    info = json.loads(METADATA.read_text())
    if compiler() != {key: info[key] for key in ("compiler", "host")}:
        raise RuntimeError("The Rust toolchain changed; run just pgo-build again")
    if info["source"] != source_hash() or info["flags"] != inherited_flags():
        raise RuntimeError("The source or compiler flags changed; run just pgo-build again")
    if hashlib.sha256(binary(info).read_bytes()).hexdigest() != info["binary"]:
        raise RuntimeError("The instrumented binary changed; run just pgo-build again")
    return info


def training_files():
    project = ARTIFACTS / "training"
    project.mkdir(parents=True, exist_ok=True)
    config = project / "config.toml"
    config.write_text('''theme = "default"
[editor]
auto-info = false
idle-timeout = 50
completion-timeout = 20
[editor.lsp]
enable = false
[editor.word-completion]
enable = true
trigger-length = 3
[editor.cursor-smear]
enabled = true
duration = 120
max-distance = 40
''')
    for number in range(12):
        lines = 24_000 if number < 2 else 100
        source = "use std::collections::HashMap;\nfn main() {\n" + "".join(
            f"    let request_handler_{number}_{i} = response_buffer_{number}_{i} + {i}; // handler request\n"
            for i in range(lines)
        ) + "}\n"
        (project / f"fixture_{number}.rs").write_text(source)
    (project / "README.md").write_text("# Profiling fixture\n\nTyping, scrolling, searching, pickers and word completion.\n" * 200)
    return project, config


def train_terminal(info, project, config, terminal):
    if os.name != "posix":
        raise RuntimeError("Automatic PTY training needs Linux/macOS; train the instrumented binary manually and run just pgo-merge")
    import fcntl
    import pty
    import termios

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 1200, 800))
    env = os.environ.copy()
    env.update(TERM=terminal, TERM_PROGRAM="kitty" if terminal == "xterm-kitty" else "ghostty" if terminal == "xterm-ghostty" else "",
               COLORTERM="truecolor", HELIX_RUNTIME=str(ROOT / "runtime"),
               LLVM_PROFILE_FILE=str(RAW / "helix-%m-%p.profraw"))
    for name in ("TMUX", "STY", "ZELLIJ"):
        env.pop(name, None)
    # Isolate caches/trust state and do not write to the user's editor config.
    for kind in ("CONFIG", "CACHE", "DATA"):
        env[f"XDG_{kind}_HOME"] = str(project / kind.lower())
    process = None
    captured = bytearray()

    def drain(seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], min(0.02, max(0, deadline - time.monotonic())))
            if ready:
                try:
                    data = os.read(master, 65536)
                except OSError:
                    return
                if not data:
                    return
                captured.extend(data)

    def key(keys, delay=0.15):
        os.write(master, keys.encode())
        drain(delay)
        if process.poll() is not None:
            raise RuntimeError(f"Training editor exited early ({terminal}, status {process.returncode})")

    try:
        process = subprocess.Popen(
            [str(binary(info)), "-c", str(config), "--log", str(project / f"{terminal}.log"),
             str(project / "fixture_0.rs"), str(project / "fixture_1.rs"), str(project / "README.md")],
            cwd=project, stdin=slave, stdout=slave, stderr=slave, env=env,
        )
        os.close(slave)
        slave = None
        drain(2)
        for _ in range(3):
            key("gg")
            key("irequest_handler_", 0.4)  # Score a large word index.
            key("\x1b")
            key("\x1b")
            key("u")
            for keys in ("80j", "40l", "gh", "G", "gg", "\x04", "\x15"):
                key(keys)
            key("/handler\r")
            key("nnnNNN")
            key(" f", 0.25)
            key("fixture_1", 0.25)
            key("\x1b")
            key(" b", 0.25)
            key("fixture", 0.25)
            key("\x1b")
            key(":buffer-next\r", 0.25)
            key("\x17v")
            key("\x17q")
        os.write(master, b"\x1b")
        drain(0.15)
        os.write(master, b":qa!\r")
        deadline = time.monotonic() + 10
        while process.poll() is None and time.monotonic() < deadline:
            drain(0.1)
        if process.poll() is None or process.returncode != 0:
            raise RuntimeError(f"Training did not exit cleanly ({terminal}); see target/pgo/training/{terminal}.pty")
        print(f"Trained {terminal}: typing, scrolling, search, pickers, completion and split views", flush=True)
    finally:
        (project / f"{terminal}.pty").write_bytes(captured)
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        os.close(master)
        if slave is not None:
            os.close(slave)


def merge():
    info = checked_build()
    profiles = sorted(RAW.glob("helix-*.profraw"))
    if not profiles:
        raise RuntimeError("No training profiles found; run just pgo-train or train the instrumented binary manually")
    subprocess.run([str(llvm_profdata(info)), "merge", "-o", str(MERGED), *map(str, profiles)], check=True)
    subprocess.run([str(llvm_profdata(info)), "show", str(MERGED)], check=True)
    print(f"Merged {len(profiles)} training profiles into {MERGED}", flush=True)


def train():
    info = checked_build()
    project, config = training_files()
    for terminal in ("xterm-256color", "xterm-kitty", "xterm-ghostty"):
        train_terminal(info, project, config, terminal)
    merge()


def install(install_root):
    info = checked_build()
    if not MERGED.is_file():
        raise RuntimeError("No merged profile found; run just pgo-train or just pgo-merge first")
    cargo(["install", "--path", "helix-term", "--force"], OPTIMIZED, info, "use", install_root)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("build", "train", "merge", "install", "all"))
    parser.add_argument("--root", help="Cargo installation root (defaults to Cargo's configured root)")
    args = parser.parse_args()
    if args.action in ("build", "all"):
        build()
    if args.action in ("train", "all"):
        train()
    if args.action == "merge":
        merge()
    if args.action in ("install", "all"):
        install(args.root)


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        sys.exit(f"PGO: {error}")
