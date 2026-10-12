#!/usr/bin/env python3
"""Exercise fresh interactive fleet shells without touching existing sessions."""
import errno
import os
from pathlib import Path
import pty
import re
import select
import subprocess
import time

SSH = ["ssh", "-tt", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes",
       "-o", "UpdateHostKeys=no", "-o", "ConnectTimeout=8"]
SSH_CONFIG = Path(__file__).resolve().parent.parent / ".fleet-ssh-config"
if SSH_CONFIG.is_file():
    SSH += ["-F", str(SSH_CONFIG)]
HOSTS = [
    ("studio", "STUDIO", ["/bin/zsh", "-il"]),
    ("m3pro", "M3PRO", SSH + ["m3pro"]),
    ("hscale", "HSCALE", SSH + ["hscale"]),
    ("vps", "VPS", SSH + ["vps"]),
]
ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07]*(?:\x07|\x1b\\)")


def exercise(command, label, marks=False):
    master, slave = pty.openpty()
    process = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave,
                               start_new_session=True)
    os.close(slave)
    transcript = b""
    sent = False
    deadline = time.monotonic() + 45
    try:
        while time.monotonic() < deadline:
            if not select.select([master], [], [], 0.2)[0]:
                if process.poll() is not None:
                    break
                continue
            try:
                chunk = os.read(master, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    break
                raise
            if not chunk:
                break
            transcript += chunk
            if not sent and label.encode() in transcript:
                # Assemble markers so terminal input echo cannot satisfy the check.
                command_line = (
                    "printf '\\n__FLEET_%s__:%s\\n' CONFIG \"$STARSHIP_CONFIG\"; "
                    "starship --version; "
                    "printf '\\n__FLEET_%s__\\n' SUCCESS; starship prompt --status=0; "
                    "printf '\\n__FLEET_%s__\\n' FAILURE; starship prompt --status=1; "
                    "printf '\\n__FLEET_%s__\\n' END; exit\n"
                )
                os.write(master, command_line.encode())
                sent = True
        if process.poll() is None:
            process.wait(timeout=3)
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        os.close(master)
    text = transcript.decode(errors="replace").replace("\r", "")
    assert sent, f"No {label} in actual startup prompt:\n{text}"
    assert process.returncode == 0, f"Shell failed ({process.returncode}):\n{text}"
    assert re.search(r"__FLEET_CONFIG__:[^\n]+/\.config/starship/fleet\.toml\n", text), text
    success = text.split("__FLEET_SUCCESS__\n", 1)[1].split("__FLEET_FAILURE__\n", 1)[0]
    failure = text.split("__FLEET_FAILURE__\n", 1)[1].split("__FLEET_END__\n", 1)[0]
    assert label in ANSI.sub("", success), success
    assert label in ANSI.sub("", failure), failure
    assert success != failure, f"No failure indicator in {label} prompt"
    assert "\x1b[" in success, f"No color/style in {label} prompt"
    assert "[WARN]" not in text and "[ERROR]" not in text, text
    if marks:
        for mark in ("A", "B", "C", "D"):
            assert f"\x1b]133;{mark}" in text, f"Missing OSC-133 {mark}:\n{text}"
    def visible(value):
        return ANSI.sub("", value).replace("%{%}", "").replace("\\[\\]", "").strip()
    return success, visible(success), visible(failure)


styles = []
for name, label, command in HOSTS:
    raw, success, failure = exercise(command, label)
    style = tuple(re.findall(r"\x1b\[[0-9;]+m", raw))
    assert style not in styles, f"Duplicate color/style for {name}"
    styles.append(style)
    print(f"PASS {name}: interactive startup, selected config, colors, success/failure")
    print(f"  success: {success}\n  failure: {failure}")
    if name in ("hscale", "vps"):
        exercise(command + ["bash", "-i"], label)
        print(f"PASS {name}: non-login interactive Bash")
    if name == "vps":
        exercise(command + ["env", "TUIOS_ENV=1", "bash", "-il"], label, marks=True)
        print("PASS vps: existing OSC-133 A/B/C/D marks preserved")
