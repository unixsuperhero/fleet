#!/usr/bin/env bash
# Run foreground across native app restarts, or pass -- command [arguments...].
# The emitted env.sh and manifest.json belong to this fixture's private directory.
set -euo pipefail
exec python3 - "$@" <<'PY'
import contextlib
import json
import os
import pathlib
import pwd
import shlex
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time

if os.geteuid() == 0:
    raise SystemExit('Run as the normal account, never root.')
os.umask(0o077)
root = pathlib.Path(tempfile.mkdtemp(prefix='fleet-smoke-', dir='/tmp'))
children, servers, logs = [], [], []
setup_fixture = os.environ.get("FLEET_SMOKE_SETUP") == "1"
owned_native = None
tmux_socket = root / ('tmux-' + str(os.getuid())) / 'default'
tmux = shutil.which('tmux')
sshd = shutil.which('sshd') or '/usr/sbin/sshd'
ssh = shutil.which('ssh') or '/usr/bin/ssh'
keygen = shutil.which('ssh-keygen') or '/usr/bin/ssh-keygen'
user = pwd.getpwuid(os.getuid()).pw_name
# On Darwin this held, bound non-listening socket times out TCP connects.
# Keep it open throughout the fixture so the port cannot be reused.
reserved = socket.socket()
reserved.bind(('127.0.0.1', 0))
timeout_port = reserved.getsockname()[1]
manifest = None


def run(argv):
    result = subprocess.run(argv, text=True, capture_output=True, timeout=15)
    if result.returncode:
        raise RuntimeError(shlex.join(argv) + '\n' + result.stderr + result.stdout)
    return result


def write(name, text, mode=0o600):
    path = root / name
    path.write_text(text)
    path.chmod(mode)
    return path


def stop(child):
    if child.poll() is not None:
        return
    table = {}
    for line in run(['/bin/ps', '-axo', 'pid=,ppid=,uid=']).stdout.splitlines():
        pid, ppid, uid = map(int, line.split())
        table[pid] = (ppid, uid)
    descendants = []
    frontier = [child.pid]
    while frontier:
        parent = frontier.pop()
        for pid, (ppid, uid) in table.items():
            if ppid == parent and uid == os.getuid():
                descendants.append(pid)
                frontier.append(pid)
    # Only this live Popen child's descendants. No name/user-wide process kills.
    for pid in reversed(descendants):
        current = subprocess.run(['/bin/ps', '-p', str(pid), '-o', 'ppid=,uid='], text=True, capture_output=True, timeout=5)
        fields = current.stdout.split()
        if len(fields) == 2 and tuple(map(int, fields)) == table[pid]:
            with contextlib.suppress(ProcessLookupError):
                os.kill(pid, signal.SIGTERM)
    with contextlib.suppress(ProcessLookupError):
        os.killpg(child.pid, signal.SIGTERM)
    try:
        child.wait(timeout=5)
    except subprocess.TimeoutExpired:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(child.pid, signal.SIGKILL)
        child.wait()


def native_identity(pid):
    result = subprocess.run(['/bin/ps', '-p', str(pid), '-o', 'uid=,lstart=,comm='], text=True, capture_output=True, timeout=5)
    return result.stdout.strip() if result.returncode == 0 else None

def record_native():
    global owned_native
    path = root / 'runtime/tuios/tuios.sock.pid'
    if not setup_fixture or owned_native is not None or not path.is_file() or path.is_symlink():
        return
    text = path.read_text().strip()
    if not text.isdecimal():
        raise RuntimeError('Invalid private daemon PID file.')
    pid = int(text)
    identity = native_identity(pid)
    if identity and identity.split()[0] == str(os.getuid()):
        owned_native = (pid, identity)

def stop_native():
    record_native()
    if owned_native is None:
        return
    pid, identity = owned_native
    if native_identity(pid) != identity:
        return
    os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic() + 5
    while native_identity(pid) == identity and time.monotonic() < deadline:
        time.sleep(0.05)
    if native_identity(pid) == identity:
        os.kill(pid, signal.SIGKILL)

def preserve_proof():
    available = []
    for name in ('smoke-proof.json', 'transfer-proof.json', 'native-screenshot.png', 'native-smoke.log', 'native-setup-driver.log'):
        source = root / 'profile' / name
        if source.is_file() and not source.is_symlink():
            available.append((source, name))
    if manifest is None and not available:
        return
    destination = pathlib.Path(tempfile.mkdtemp(prefix='fleet-smoke-proof-', dir='/tmp'))
    for source, name in available:
        shutil.copyfile(source, destination / name)
        (destination / name).chmod(0o600)
    if manifest is not None:
        facts = {key: manifest[key] for key in ('hostKeyAlgorithm', 'hostKeyFingerprint', 'checks', 'masterPid', 'ports')}
        facts['fixtureUid'] = os.getuid()
        facts['masterAliveBeforeCleanup'] = master.poll() is None
        (destination / 'fixture-proof.json').write_text(json.dumps(facts, indent=2) + '\n')
    print(json.dumps({'status': 'proof-preserved', 'directory': str(destination)}), flush=True)


def cleanup():
    with contextlib.suppress(OSError, RuntimeError):
        preserve_proof()
    with contextlib.suppress(OSError, RuntimeError, subprocess.SubprocessError):
        stop_native()
    for child in reversed(children):
        with contextlib.suppress(OSError, subprocess.SubprocessError):
            stop(child)
    # This private socket is created by our tmux wrapper, not the user's server.
    if tmux and tmux_socket.exists():
        with contextlib.suppress(subprocess.SubprocessError):
            subprocess.run([tmux, '-S', str(tmux_socket), 'kill-server'], capture_output=True, timeout=5)
    reserved.close()
    for log in logs:
        log.close()
    shutil.rmtree(root)


def interrupted(signum, _):
    raise SystemExit(128 + signum)


signal.signal(signal.SIGTERM, interrupted)
signal.signal(signal.SIGINT, interrupted)
try:
    if not tmux:
        raise RuntimeError('Installed tmux required. Nothing will be installed.')
    for name in ('hostkey', 'wrong-hostkey', 'clientkey', 'agentkey', 'unauthorized-clientkey'):
        run([keygen, '-q', '-t', 'ed25519', '-N', '', '-C', 'fleet-isolated-fixture', '-f', str(root / name)])
    host_pub = (root / 'hostkey.pub').read_text().split()
    wrong_pub = (root / 'wrong-hostkey.pub').read_text().split()
    fingerprint = run([keygen, '-lf', str(root / 'hostkey.pub'), '-E', 'sha256']).stdout.split()[1]
    write('authorized_keys', (root / 'clientkey.pub').read_text() + (root / 'agentkey.pub').read_text())
    agent_socket = root / 'agent.sock'
    agent_log = open(root / 'agent.log', 'ab', buffering=0)
    logs.append(agent_log)
    agent = subprocess.Popen(['/usr/bin/ssh-agent', '-D', '-a', str(agent_socket)], stdout=agent_log, stderr=agent_log, start_new_session=True)
    children.append(agent)
    for _ in range(50):
        if agent.poll() is not None:
            raise RuntimeError('Private fixture ssh-agent exited: ' + (root / 'agent.log').read_text())
        if agent_socket.exists():
            break
        time.sleep(0.1)
    else:
        raise RuntimeError('Private fixture ssh-agent socket did not become ready.')
    run(['/usr/bin/env', 'SSH_AUTH_SOCK=' + str(agent_socket), '/usr/bin/ssh-add', str(root / 'agentkey')])
    for directory in ('bin', 'missing-bin'):
        (root / directory).mkdir()
    for name in ('sh', 'bash', 'env', 'uname', 'sysctl', 'hostname', 'cat', 'date', 'od', 'tr', 'sed', 'cut', 'awk', 'grep', 'wc', 'head', 'tail', 'mkdir', 'mktemp', 'rm', 'dirname', 'sleep', 'stty', 'tty', 'printf', 'ps', 'id', 'kill', 'readlink', 'yes'):
        executable = shutil.which(name)
        if executable:
            for directory in ('bin', 'missing-bin'):
                (root / directory / name).symlink_to(executable)
    write('bin/tmux', '#!/bin/sh\nexec ' + shlex.quote(tmux) + ' -f /dev/null -S ' + shlex.quote(str(tmux_socket)) + ' "$@"\n', 0o700)
    setup_environment = ""
    if setup_fixture:
        for name in ("runtime", "state", "config"):
            (root / name).mkdir(mode=0o700)
        native_tuios = shutil.which("tuios") or str(pathlib.Path.home() / "bin" / "tuios")
        for name, executable in (("bun", shutil.which("bun")), ("tuios", native_tuios)):
            if not executable or not pathlib.Path(executable).is_file():
                raise RuntimeError("Installed real " + name + " required for the setup fixture.")
            (root / "bin" / name).symlink_to(executable)
        setup_environment = "".join("export " + key + "=" + shlex.quote(str(root / value)) + "\n" for key, value in (("XDG_RUNTIME_DIR", "runtime"), ("XDG_STATE_HOME", "state"), ("XDG_CONFIG_HOME", "config")))
    for mode in ('normal', 'missing'):
        directory = root / ('bin' if mode == 'normal' else 'missing-bin')
        sandbox = '/usr/bin/sandbox-exec -p ' + shlex.quote('(version 1)(allow default)(deny file-read* (literal "/opt/homebrew/bin/tmux") (literal "/usr/local/bin/tmux") (literal "/usr/bin/tmux"))') + ' ' if mode == 'missing' else ''
        write('shell-' + mode, '#!/bin/sh\nunset TMUX\nexport TMUX_TMPDIR=' + shlex.quote(str(root)) + '\nexport HOME=' + shlex.quote(str(root)) + '\nexport PATH=' + shlex.quote(str(directory)) + '\nexport LC_ALL=en_US.UTF-8\n' + setup_environment + 'cd "$HOME" || exit 1\nif [ -n "$SSH_ORIGINAL_COMMAND" ]; then exec ' + sandbox + '/bin/sh -c "$SSH_ORIGINAL_COMMAND"; fi\nexec ' + sandbox + '/bin/sh -i\n', 0o700)
    ports = {}
    for name, mode in (('normal', 'normal'), ('missing', 'missing'), ('denied', 'normal')):
        for attempt in range(5):
            with socket.socket() as candidate:
                candidate.bind(('127.0.0.1', 0))
                port = candidate.getsockname()[1]
            lines = [
                'ListenAddress 127.0.0.1', 'Port ' + str(port),
                'HostKey ' + str(root / 'hostkey'), 'PidFile ' + str(root / (name + '.pid')),
                'AuthorizedKeysFile ' + str(root / 'authorized_keys'),
                # /tmp's parent permissions make StrictModes unsuitable here.
                # All fixture-controlled directories/files are 0700/0600 instead.
                'StrictModes no', 'AllowUsers ' + user, 'UsePAM no',
                'PubkeyAuthentication yes', 'AuthenticationMethods publickey',
                'PasswordAuthentication no', 'KbdInteractiveAuthentication no',
                'PermitEmptyPasswords no', 'PermitRootLogin no', 'UseDNS no',
                'PrintMotd no', 'PrintLastLog no', 'DisableForwarding yes',
                'PermitUserEnvironment no', 'PermitUserRC no', 'LoginGraceTime 10',
                'LogLevel VERBOSE', 'PerSourcePenalties no',
                'ForceCommand ' + str(root / ('shell-' + mode))
            ]
            if name == 'denied':
                lines.append('MaxSessions 0')
            config = write('sshd-' + name + '.conf', '\n'.join(lines) + '\n')
            run([sshd, '-t', '-f', str(config)])
            log = open(root / (name + '.log'), 'ab', buffering=0)
            logs.append(log)
            daemon = subprocess.Popen([sshd, '-D', '-e', '-f', str(config)], stdout=log, stderr=log, start_new_session=True)
            children.append(daemon)
            ready = False
            for _ in range(50):
                if daemon.poll() is not None:
                    break
                try:
                    with socket.create_connection(('127.0.0.1', port), timeout=0.2) as connection:
                        connection.settimeout(0.2)
                        ready = connection.recv(128).startswith(b'SSH-')
                except OSError:
                    pass
                if ready:
                    break
                time.sleep(0.1)
            if ready:
                ports[name] = port
                servers.append(daemon)
                break
            stop(daemon)
        else:
            raise RuntimeError('Nonroot sshd startup failed. Exact server log:\n' + (root / (name + '.log')).read_text())
    aliases = {
        'valid': 'fleet-smoke-valid', 'agentAuth': 'fleet-smoke-agent-auth', 'unknownKey': 'fleet-smoke-unknown-key',
        'changedKey': 'fleet-smoke-changed-key', 'authFailure': 'fleet-smoke-auth-failure',
        'missingTmux': 'fleet-smoke-missing-tmux', 'dnsFailure': 'fleet-smoke-dns-failure',
        'tcpTimeout': 'fleet-smoke-tcp-timeout', 'authSessionDenied': 'fleet-smoke-session-denied',
        'permissiveUnknownKey': 'fleet-smoke-permissive-unknown', 'existingMaster': 'fleet-smoke-master'
    }
    if setup_fixture:
        aliases["setupRoute"] = "fleet-smoke-setup-route"
    blocks = []
    for kind, alias in aliases.items():
        trusted = wrong_pub if kind == 'changedKey' else host_pub
        empty = kind in ('unknownKey', 'permissiveUnknownKey', 'existingMaster', 'dnsFailure', 'tcpTimeout')
        key_alias = "fleet-smoke-setup-trusted" if kind == "setupRoute" else alias
        known = write(kind + '.known_hosts', '' if empty else key_alias + ' ' + ' '.join(trusted[:2]) + '\n')
        port = ports['denied'] if kind == 'setupRoute' else timeout_port if kind == 'tcpTimeout' else ports['denied' if kind == 'authSessionDenied' else 'missing' if kind == 'missingTmux' else 'normal']
        lines = [
            'Host ' + alias,
            '  HostName ' + ('fleet-smoke-does-not-exist.invalid' if kind in ('dnsFailure', 'setupRoute') else '127.0.0.1'),
            '  Port ' + str(port), '  User ' + user, '  HostKeyAlias ' + ('fleet-smoke-setup-wrong-key' if kind == 'setupRoute' else alias),
            '  UserKnownHostsFile ' + str(known),
            '  IdentityFile ' + str(root / ('agentkey.pub' if kind == 'agentAuth' else 'unauthorized-clientkey' if kind == 'authFailure' else 'clientkey'))
        ]
        if kind == 'agentAuth':
            lines.append('  IdentityAgent ' + str(agent_socket))
        if kind == 'permissiveUnknownKey':
            lines.extend(['  StrictHostKeyChecking no', '  NoHostAuthenticationForLocalhost yes'])
        if kind == 'existingMaster':
            lines.extend(['  ControlPath ' + str(root / 'master.sock'), '  ControlMaster no'])
        blocks.append('\n'.join(lines))
    blocks.append('\n'.join([
        'Host fleet-smoke-*', '  GlobalKnownHostsFile /dev/null',
        '  IdentityAgent none', '  IdentitiesOnly yes', '  AddKeysToAgent no',
        '  StrictHostKeyChecking ask', '  NoHostAuthenticationForLocalhost no',
        '  VerifyHostKeyDNS no', '  FingerprintHash sha256', '  UpdateHostKeys no',
        '  CheckHostIP no', '  ConnectTimeout 5', '  ConnectionAttempts 1',
        '  ForwardAgent no', '  ForwardX11 no', '  ClearAllForwardings yes',
        '  ProxyCommand none', '  ProxyJump none', '  CanonicalizeHostname no',
        '  ControlMaster no', '  ControlPath none', '  ControlPersist no',
        '  PreferredAuthentications publickey'
    ]))
    # Include existing configuration only for non-fixture targets. It is not edited.
    user_config = pathlib.Path.home() / '.ssh' / 'config'
    if user_config.is_file():
        blocks.append('Host * !fleet-smoke-*\n  Include "' + str(user_config).replace('"', '\\"') + '"')
    client_config = write('ssh_config', '\n\n'.join(blocks) + '\n')
    checks = {}
    for kind in ('valid', 'agentAuth', 'unknownKey', 'changedKey', 'authFailure', 'missingTmux', 'dnsFailure', 'tcpTimeout', 'authSessionDenied'):
        command = 'command -v tmux || exit 127' if kind == 'missingTmux' else "printf '%s\\n' FLEET_FIXTURE_AUTHENTICATED"
        arguments = [ssh, '-F', str(client_config), '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes']
        if kind == 'authSessionDenied':
            arguments += ['-o', 'PermitLocalCommand=yes', '-o', 'LocalCommand=printf FLEET_FIXTURE_CLIENT_AUTHENTICATED']
        result = subprocess.run(arguments + [aliases[kind], command], capture_output=True, text=True, timeout=15)
        checks[kind] = {'exitCode': result.returncode, 'stdout': result.stdout.strip(), 'stderr': result.stderr.strip()}
        expected = {'valid': 0, 'agentAuth': 0, 'unknownKey': 255, 'changedKey': 255, 'authFailure': 255, 'missingTmux': 127, 'dnsFailure': 255, 'tcpTimeout': 255, 'authSessionDenied': 255}[kind]
        phrase = {
            'unknownKey': 'Host key verification failed',
            'changedKey': 'REMOTE HOST IDENTIFICATION HAS CHANGED',
            'authFailure': 'Permission denied', 'dnsFailure': 'Could not resolve hostname',
            'tcpTimeout': 'Operation timed out', 'authSessionDenied': 'channel 0: open failed'
        }.get(kind)
        if result.returncode != expected or (phrase and phrase not in result.stderr):
            raise RuntimeError('Fixture outcome/stage mismatch for ' + kind + ': ' + json.dumps(checks[kind]))
        if kind == 'authSessionDenied' and result.stdout.strip() != 'FLEET_FIXTURE_CLIENT_AUTHENTICATED':
            raise RuntimeError('Session refusal was not observed after authenticated client readiness.')
        if kind in ('valid', 'agentAuth') and result.stdout.strip() != 'FLEET_FIXTURE_AUTHENTICATED':
            raise RuntimeError('Authenticated command execution not proved.')
    if (root / 'unknownKey.known_hosts').stat().st_size:
        raise RuntimeError('Unknown key was unexpectedly accepted.')
    master_log = open(root / 'master.log', 'ab', buffering=0)
    logs.append(master_log)
    master = subprocess.Popen([
        ssh, '-F', str(client_config), '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes',
        '-o', 'ControlMaster=yes', '-o', 'ControlPath=' + str(root / 'master.sock'),
        '-o', 'ControlPersist=no', '-MN', aliases['valid']
    ], stdout=master_log, stderr=master_log, start_new_session=True)
    children.append(master)
    for _ in range(50):
        if (root / 'master.sock').exists():
            break
        if master.poll() is not None:
            raise RuntimeError('Fixture multiplexing master failed: ' + (root / 'master.log').read_text())
        time.sleep(0.1)
    else:
        raise RuntimeError('Fixture master socket did not become ready.')
    profile = root / 'profile'
    profile.mkdir()
    write('corrupt.sqlite3', 'This is not a SQLite database.\n')
    manifest = {
        'root': str(root), 'config': str(client_config), 'profile': str(profile),
        'hostKeyAlgorithm': 'ssh-ed25519', 'hostKeyFingerprint': fingerprint,
        'aliases': aliases, 'checks': checks, 'ports': {**ports, 'timeout': timeout_port},
        'masterPid': master.pid, 'nativeBackup': str(root / 'native-backup.sqlite3'),
        'corruptBackup': str(root / 'corrupt.sqlite3'), 'newerBackup': str(root / 'newer.sqlite3')
    }
    if setup_fixture:
        manifest["setup"] = {"runtime": str(root / "runtime"), "state": str(root / "state"), "config": str(root / "config"), "home": str(root), "route": aliases["setupRoute"], "hostKeyAlias": "fleet-smoke-setup-trusted"}
    write('manifest.json', json.dumps(manifest, indent=2) + '\n')
    environment = dict(os.environ, FLEET_SMOKE_SSH_CONFIG=str(client_config), FLEET_PROFILE_DIR=str(profile), FLEET_SMOKE_FIXTURE_MANIFEST=str(root / 'manifest.json'))
    write('env.sh', ''.join('export ' + key + '=' + shlex.quote(environment[key]) + '\n' for key in ('FLEET_SMOKE_SSH_CONFIG', 'FLEET_PROFILE_DIR', 'FLEET_SMOKE_FIXTURE_MANIFEST')))
    print(json.dumps({'status': 'ready', **manifest}), flush=True)
    print('source ' + shlex.quote(str(root / 'env.sh')), file=sys.stderr, flush=True)
    args = sys.argv[1:]
    if args and args[0] == '--':
        args = args[1:]
    app = None
    if args:
        app = subprocess.Popen(args, env=environment, start_new_session=True)
        children.append(app)
    else:
        print('Keep this foreground fixture alive across app restarts. Ctrl-C cleans its own processes and private tmux socket.', file=sys.stderr)
    while app is None or app.poll() is None:
        if setup_fixture:
            record_native()
        if master.poll() is not None:
            raise RuntimeError('Fixture existing master was terminated unexpectedly.')
        if any(server.poll() is not None for server in servers):
            raise RuntimeError('Fixture sshd exited unexpectedly.')
        backup = root / 'native-backup.sqlite3'
        if backup.exists() and not (root / 'newer.sqlite3').exists():
            try:
                with sqlite3.connect(backup.as_uri() + '?mode=ro', uri=True) as source:
                    if source.execute('PRAGMA integrity_check').fetchone()[0] != 'ok':
                        raise RuntimeError('Native-exported backup failed integrity_check.')
                    if source.execute('PRAGMA application_id').fetchone()[0] != 0x464c4554:
                        raise RuntimeError('Native export is not a Fleet database.')
                    version = source.execute('PRAGMA user_version').fetchone()[0]
                    if version == 0:
                        raise sqlite3.DatabaseError('Export is not ready.')
                    with sqlite3.connect(root / 'newer.sqlite3') as destination:
                        source.backup(destination)
                        destination.execute('PRAGMA user_version = ' + str(version + 1))
                print(json.dumps({'status': 'restore-fixtures-ready', 'newerSchema': version + 1}), flush=True)
            except sqlite3.DatabaseError:
                (root / 'newer.sqlite3').unlink(missing_ok=True)
        time.sleep(0.2)
    if master.poll() is not None:
        raise RuntimeError('Fixture master died during native command execution.')
    raise SystemExit(app.returncode)
except (OSError, subprocess.SubprocessError, RuntimeError) as error:
    print(str(error), file=sys.stderr)
    print('No root/sudo fallback. If nonroot sshd is denied, retain the exact error above. Approved hscale/M3/vps can prove ordinary SSH, not isolated changed/unknown-key cases. Those require a separately approved unprivileged OpenSSH environment. No OS reboot or fresh-account claim is made.', file=sys.stderr)
    raise SystemExit(1)
finally:
    cleanup()
PY
