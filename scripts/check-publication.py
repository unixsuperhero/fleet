#!/usr/bin/env python3
"""Check publishable files or Git history without printing sensitive values."""
import argparse
import ipaddress
import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def git(*args):
    return subprocess.check_output(["git", "-C", str(ROOT), *args])


PATTERNS = {
    "private key": re.compile(rb"-----BEGIN (?:OPENSSH |RSA |EC |DSA |ENCRYPTED )?PRIVATE KEY-----|PuTTY-User-Key-File-[23]:"),
    "provider credential": re.compile(rb"(?:AKIA|ASIA)[A-Z0-9]{16}|gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,}|sk-(?:proj-|ant-)?[A-Za-z0-9_-]{30,}|xox[baprs]-[A-Za-z0-9-]{20,}|AIza[A-Za-z0-9_-]{35}"),
    "private node domain": re.compile(rb"(?i)[\x22\x27`][a-z0-9_-]+\.node\.(?!example\b)[a-z0-9.-]+"),
    "personal signing identity": re.compile(rb"Developer ID Application: [^\r\n\x22\x27]+\([A-Z0-9]{10}\)"),
}
IPV4 = re.compile(rb"(?<![\w.])(?:\d{1,3}\.){3}\d{1,3}(?![\w.])")
IPV6 = re.compile(rb"(?i)(?<![\w:])[a-f0-9]*:[a-f0-9:]+(?:\.[0-9.]+)?(?=$|[\s\x22\x27`\]\),;.])")
EXAMPLE_V4 = tuple(ipaddress.ip_network(value) for value in ("192.0.2.0/24", "198.51.100.0/24", "203.0.113.0/24"))
EXAMPLE_V6 = ipaddress.ip_network("2001:db8::/32")


def scan(data, label):
    failures = []
    if b"\0" in data[:8192]:
        return failures
    for number, line in enumerate(data.splitlines(), 1):
        for kind, pattern in PATTERNS.items():
            if pattern.search(line):
                failures.append((label, number, kind))
        for pattern in (IPV4, IPV6):
            for match in pattern.finditer(line):
                try:
                    address = ipaddress.ip_address(match.group().decode())
                except ValueError:
                    continue
                if address.is_loopback or address.is_unspecified:
                    continue
                if address.version == 4 and any(address in network for network in EXAMPLE_V4):
                    continue
                if address.version == 6 and (address in EXAMPLE_V6 or address.ipv4_mapped and any(address.ipv4_mapped in network for network in EXAMPLE_V4)):
                    continue
                failures.append((label, number, "non-example IP address"))
    return failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--history", action="store_true", help="Scan every unique blob reachable from local Git refs instead of working files.")
    args = parser.parse_args()
    failures = []
    checked = 0
    if args.history:
        for row in git("rev-list", "--objects", "--all").decode().splitlines():
            oid, _, name = row.partition(" ")
            if git("cat-file", "-t", oid).strip() != b"blob":
                continue
            failures.extend(scan(git("cat-file", "blob", oid), f"{name}@{oid[:12]}"))
            checked += 1
    else:
        for name in git("ls-files", "-z").decode().split("\0"):
            if not name:
                continue
            path = ROOT / name
            if not path.exists():
                continue
            if path.is_symlink():
                failures.append((name, 0, "symlink requires manual review"))
                continue
            if path.name in (".env", ".fleet-ssh-config") or path.suffix in (".pem", ".key", ".p12", ".pfx", ".db", ".sqlite", ".sqlite3"):
                failures.append((name, 0, "private file is tracked"))
            failures.extend(scan(path.read_bytes(), name))
            checked += 1
    for label, number, kind in sorted(set(failures)):
        print(f"{label}:{number}: {kind}")
    print(f"Checked {checked} {'historical blobs' if args.history else 'tracked files'}; {len(set(failures))} findings.")
    return bool(failures)


if __name__ == "__main__":
    raise SystemExit(main())
