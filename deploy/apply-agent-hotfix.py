#!/usr/bin/env python3
"""Atomically replace a qualified agent without rewriting its configuration.

Run as root; verify the installed and executing binary fingerprints first.
Backup contains the prior binary, complete configuration and service states.
Explicit or automatic rollback restores only the binary, preserving any later
user configuration edits. No service enablement or desktop setting changes.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import pwd
import shutil
import subprocess
import time
import tomllib


def digest(path):
    with path.open('rb') as stream: return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--user', required=True)
    parser.add_argument('--candidate', type=Path)
    parser.add_argument('--expected-current-sha256')
    parser.add_argument('--expected-candidate-sha256')
    parser.add_argument('--backup-id')
    parser.add_argument('--rollback', type=Path)
    args = parser.parse_args()
    assert os.geteuid() == 0
    account = pwd.getpwnam(args.user); home = Path(account.pw_dir)
    installed = home/'.local/bin/poolsync-agent'
    config = home/'.config/poolsync/agent.toml'
    runtime = '/run/user/'+str(account.pw_uid)
    units = ['poolsync-agent.service', 'poolsync-watchdog.timer', 'poolsync-watchdog.service']
    def service(*arguments, check=False):
        return subprocess.run(['/usr/sbin/runuser', '-u', args.user, '--', '/usr/bin/env',
                               'XDG_RUNTIME_DIR='+runtime, 'DBUS_SESSION_BUS_ADDRESS=unix:path='+runtime+'/bus',
                               '/usr/bin/systemctl', '--user', *arguments], capture_output=True, text=True, timeout=25, check=check)
    def atomic(source):
        temp = installed.with_name('poolsync-agent.hotfix-new')
        shutil.copyfile(source, temp); os.chown(temp, account.pw_uid, account.pw_gid); temp.chmod(0o755)
        with temp.open('rb') as stream: os.fsync(stream.fileno())
        os.replace(temp, installed)
    def running(expected):
        pid = service('show', 'poolsync-agent.service', '--property=MainPID', '--value', check=True).stdout.strip()
        if not pid or pid == '0': return None
        try:
            if digest(Path('/proc')/pid/'exe') != expected: return None
            if (Path('/proc')/pid/'cmdline').read_bytes().split(b'\0')[0] != str(installed).encode(): return None
        except OSError: return None
        return int(pid)
    def restart(manifest):
        service('start', 'poolsync-agent.service', check=True)
        for unit in units[1:]:
            if manifest['active_units'][unit]: service('start', unit, check=True)
    def restore(backup):
        manifest = json.loads((backup/'manifest.json').read_text())
        assert manifest['user'] == args.user and manifest['installed'] == str(installed)
        assert digest(backup/'agent') == manifest['previous_sha256']
        service('stop', *units, check=True)
        atomic(backup/'agent'); restart(manifest)
        deadline = time.monotonic()+8
        while time.monotonic() < deadline:
            if running(manifest['previous_sha256']): return
            time.sleep(.1)
        raise AssertionError('rollback binary did not start')
    if args.rollback:
        restore(args.rollback); print(json.dumps({'rollback_restored': True})); return
    assert all((args.candidate, args.expected_current_sha256, args.expected_candidate_sha256, args.backup_id))
    assert all(c.isalnum() or c in '-_.' for c in args.backup_id)
    assert digest(installed) == args.expected_current_sha256 and running(args.expected_current_sha256)
    assert digest(args.candidate) == args.expected_candidate_sha256
    cfg = tomllib.loads(config.read_text()); assert cfg['hubless']
    config_sha = digest(config)
    backup = home/'.local/state/poolsync/deployment-backups'/args.backup_id
    backup.mkdir(mode=0o700); shutil.copy2(installed, backup/'agent')
    shutil.copytree(config.parent, backup/'config')
    manifest = {'user': args.user, 'installed': str(installed), 'node': cfg['node'],
                'previous_sha256': args.expected_current_sha256, 'candidate_sha256': args.expected_candidate_sha256,
                'configuration_sha256': config_sha,
                'active_units': {unit: service('is-active', unit).returncode == 0 for unit in units}}
    assert manifest['active_units']['poolsync-agent.service']
    (backup/'manifest.json').write_text(json.dumps(manifest, indent=2)+'\n'); (backup/'manifest.json').chmod(0o600)
    try:
        service('stop', *units, check=True); atomic(args.candidate); restart(manifest)
        deadline = time.monotonic()+8
        pid = None
        while time.monotonic() < deadline:
            pid = running(args.expected_candidate_sha256)
            if pid: break
            time.sleep(.1)
        assert pid, 'qualified executable did not start'
        assert digest(config) == config_sha, 'configuration changed during binary hotfix'
    except Exception:
        restore(backup); raise
    print(json.dumps({'installed': True, 'node': cfg['node'], 'pid': pid, 'running_sha256': args.expected_candidate_sha256,
                      'configuration_preserved': True, 'backup': str(backup)}))


if __name__ == '__main__': main()
