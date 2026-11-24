#!/usr/bin/env python3
"""Keep Cargo build inputs after stopping abandoned Actions cache writers."""
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

STAMP = 'target/.pinewasm-ci-inputs-v2.json'
CONFIG = ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', '.cargo/config.toml',
          'examples/embed-cli/Cargo.toml', 'examples/embed-cli/Cargo.lock')
CACHES = ('target', 'examples/embed-cli/target')
BUILD_ENV = ('RUNNER_OS', 'RUNNER_ARCH', 'PINEWASM_RUST_VERSION', 'RUSTUP_TOOLCHAIN',
             'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_TARGET',
             'CARGO_TARGET_DIR', 'CARGO_INCREMENTAL', 'RUSTC', 'RUSTC_WRAPPER',
             'RUSTC_WORKSPACE_WRAPPER')


def signature(root, environment):
    digest = hashlib.sha256()
    for name in CONFIG:
        path = root / name
        data = path.read_bytes() if path.exists() else b'<absent>'
        digest.update(name.encode() + b'\0' + data + b'\0')
    for name in BUILD_ENV:
        digest.update(name.encode() + b'\0' + environment.get(name, '').encode() + b'\0')
    return digest.hexdigest()


def warm(root, key):
    target, stamp = root / 'target', root / STAMP
    if target.is_symlink() or stamp.is_symlink():
        raise ValueError('Native cache must remain inside the dedicated checkout')
    try:
        return json.loads(stamp.read_bytes()) == {'version': 2, 'inputs': key}
    except (OSError, ValueError):
        return False


def checkpoint(root, key):
    warm(root, key)  # Reject symlinks before writing inside target.
    stamp = root / STAMP
    stamp.parent.mkdir(parents=True, exist_ok=True)
    temporary = stamp.with_suffix('.tmp')
    temporary.unlink(missing_ok=True)
    temporary.write_text(json.dumps({'version': 2, 'inputs': key}) + '\n')
    temporary.replace(stamp)


def prepare(root, key):
    for name in CACHES:
        path = root / name
        if path.is_symlink() or not path.resolve().is_relative_to(root.resolve()):
            raise ValueError('Native cache must remain inside the dedicated checkout')
    reuse = warm(root, key)
    # Compiler metadata is not evidence that the remaining outputs are coherent.
    # Reuse is accepted only for workspaces prepared under the exclusive-restore
    # guard and clean boundary.
    command = ['git', 'clean', '-ffdx']
    if reuse:
        for name in CACHES:
            command += ['-e', '/' + name + '/']
    subprocess.run(command, cwd=root, check=True)
    (root / STAMP).unlink(missing_ok=True)
    return reuse


def writers(workspace, temporary, listing):
    destinations = {' -C ' + str(p) + ' ' for p in (workspace.absolute(), workspace.resolve())}
    archives = {' -xf ' + str(p) + '/' for p in (temporary.absolute(), temporary.resolve())}
    result = []
    for line in listing.splitlines():
        fields = line.strip().split(None, 2)
        if len(fields) != 3 or not all(x.isdigit() for x in fields[:2]):
            continue
        pid, parent, command = fields
        if (Path(command.split(None, 1)[0]).name == 'tar'
                and '/cache.tzst' in command
                and any(a in command for a in archives)
                and any(d in ' ' + command + ' ' for d in destinations)):
            result.append((int(pid), int(parent), command))
    return result


def process_listing():
    return subprocess.check_output(['ps', '-axo', 'pid=,ppid=,command='], text=True)


def stop_abandoned(workspace, temporary, inspect=process_listing, stop=os.kill, pause=time.sleep):
    found = writers(workspace, temporary, inspect())
    if any(parent != 1 for _, parent, _ in found):
        raise ValueError('A live cache restore still owns this workspace; refusing concurrent cleanup')
    for record in found:
        # Recheck identity immediately before signalling, including its orphaned
        # parent. Never stop a current action or a process in another workspace.
        if record in writers(workspace, temporary, inspect()):
            try:
                stop(record[0], signal.SIGTERM)
            except ProcessLookupError:
                pass
    for _ in range(50):
        if not writers(workspace, temporary, inspect()):
            return
        pause(0.1)
    raise ValueError('Cache writer did not exit; refusing to compile into a changing directory')


def main():
    if len(sys.argv) != 2 or sys.argv[1] not in {'prepare', 'recover', 'checkpoint'}:
        raise ValueError('Expected prepare, recover or checkpoint')
    root = Path(__file__).resolve().parents[2]
    if (os.environ.get('GITHUB_ACTIONS') != 'true'
            or Path(os.environ.get('GITHUB_WORKSPACE', '')).resolve() != root):
        raise ValueError('Cleanup is restricted to this dedicated Actions checkout')
    top = subprocess.check_output(['git', 'rev-parse', '--show-toplevel'], cwd=root, text=True).strip()
    if Path(top).resolve() != root:
        raise ValueError('Actions workspace is not the Git root')
    stop_abandoned(root, Path(os.environ['RUNNER_TEMP']))
    if sys.argv[1] == 'prepare':
        # Checkout used clean:false so an abandoned tar cannot race git clean.
        # Cargo build inputs are kept when eligible. Stale untracked source is
        # removed; Cargo still checks freshness and selected tests still run.
        reuse = prepare(root, signature(root, os.environ))
        with open(os.environ['GITHUB_OUTPUT'], 'a') as output:
            output.write('warm=' + str(reuse).lower() + '\n')
        print('Kept Cargo build inputs.' if reuse else 'Cold native workspace.')
    elif sys.argv[1] == 'checkpoint':
        checkpoint(root, signature(root, os.environ))
        print('Checkpointed build inputs.')
    print('Native cache workspace has no overlapping restore writer.')


if __name__ == '__main__':
    main()
