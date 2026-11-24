"""Cache recovery must never kill another job or compile during an extraction."""
import importlib.util
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

spec = importlib.util.spec_from_file_location('native_cache', Path(__file__).with_name('native_cache.py'))
cache = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cache)


class NativeCacheTests(unittest.TestCase):
    root = Path('/tmp/runner/work space/repo')
    temporary = Path('/tmp/runner/temp')

    def listing(self, parent=1, root=None, archive=None):
        return f'42 {parent} /usr/bin/tar -xf {archive or self.temporary / "job/cache.tzst"} -P -C {root or self.root} --use-compress-program unzstd'

    def test_exact_workspace_archive_and_executable_are_required(self):
        valid = self.listing()
        listing = '\n'.join([valid, self.listing(root=Path(str(self.root)+'-other')),
                             self.listing(archive=Path('/tmp/unrelated/cache.tzst')),
                             valid.replace('/usr/bin/tar', '/bin/echo'), 'invalid process row'])
        self.assertEqual(cache.writers(self.root, self.temporary, listing), [(42, 1, valid.split(None, 2)[2])])

    def test_only_confirmed_orphan_is_stopped_and_waited_for(self):
        snapshots = iter([self.listing(), self.listing(), self.listing(), ''])
        stopped, waits = [], []
        cache.stop_abandoned(self.root, self.temporary, lambda: next(snapshots),
                             lambda *args: stopped.append(args), waits.append)
        self.assertEqual(stopped, [(42, signal.SIGTERM)])
        self.assertEqual(waits, [0.1])

    def test_live_restore_blocks_without_signalling(self):
        stopped = []
        with self.assertRaisesRegex(ValueError, 'live cache restore'):
            cache.stop_abandoned(self.root, self.temporary, lambda: self.listing(99),
                                 lambda *args: stopped.append(args))
        self.assertEqual(stopped, [])

    def test_process_identity_change_is_not_signalled(self):
        snapshots = iter([self.listing(), '', ''])
        stopped = []
        cache.stop_abandoned(self.root, self.temporary, lambda: next(snapshots),
                             lambda *args: stopped.append(args))
        self.assertEqual(stopped, [])

    def test_lingering_writer_fails_before_build_can_start(self):
        stopped = []
        with self.assertRaisesRegex(ValueError, 'did not exit'):
            cache.stop_abandoned(self.root, self.temporary, lambda: self.listing(),
                                 lambda *args: stopped.append(args), lambda _: None)
        self.assertEqual(stopped, [(42, signal.SIGTERM)])

    def test_empty_workspace_does_not_signal_anything(self):
        stopped = []
        cache.stop_abandoned(self.root, self.temporary, lambda: '', lambda *args: stopped.append(args))
        self.assertEqual(stopped, [])

    def test_cli_refuses_a_normal_local_checkout_without_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            sentinel = Path(directory) / 'keep.txt'
            sentinel.write_text('unrelated local work')
            result = subprocess.run(
                [sys.executable, '-B', str(Path(cache.__file__).resolve()), 'prepare'],
                cwd=directory, env={**os.environ, 'GITHUB_ACTIONS': 'false'},
                capture_output=True, text=True,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('restricted to this dedicated Actions checkout', result.stderr)
            self.assertEqual(sentinel.read_text(), 'unrelated local work')


class WorkspaceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        subprocess.run(['git', 'init', '-q', str(self.root)], check=True)
        for name in cache.CONFIG:
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('fixture\n')
        (self.root / 'src').mkdir()
        (self.root / 'src/main.rs').write_text('fn main() {}\n')
        subprocess.run(['git', 'add', '.'], cwd=self.root, check=True)
        (self.root / 'target').mkdir()
        (self.root / 'target/retained').write_text('compiled input')
        self.environment = {'RUNNER_OS': 'macOS', 'RUNNER_ARCH': 'ARM64'}
        self.key = cache.signature(self.root, self.environment)

    def test_warm_inputs_survive_while_stale_source_is_removed(self):
        cache.checkpoint(self.root, self.key)
        example_cache = self.root / 'examples/embed-cli/target'
        example_cache.mkdir()
        (example_cache / 'retained').write_text('compiled embedding input')
        (self.root / 'Stale.rs').write_text('untracked source')
        self.assertTrue(cache.prepare(self.root, self.key))
        self.assertEqual((self.root / 'target/retained').read_text(), 'compiled input')
        self.assertEqual((example_cache / 'retained').read_text(), 'compiled embedding input')
        self.assertFalse((self.root / 'Stale.rs').exists())
        self.assertTrue((self.root / 'src/main.rs').exists())
        self.assertFalse((self.root / cache.STAMP).exists())

    def test_pin_change_discards_old_inputs(self):
        cache.checkpoint(self.root, self.key)
        (self.root / 'rust-toolchain.toml').write_text('changed pin')
        changed = cache.signature(self.root, self.environment)
        self.assertNotEqual(changed, self.key)
        self.assertFalse(cache.prepare(self.root, changed))
        self.assertFalse((self.root / 'target').exists())
        self.assertTrue((self.root / 'src/main.rs').exists())

    def test_platform_and_compiler_flags_change_identity_but_source_does_not(self):
        for changed in [{'RUNNER_ARCH': 'X64'}, {'RUSTFLAGS': '-C opt-level=2'}]:
            self.assertNotEqual(self.key, cache.signature(self.root, {**self.environment, **changed}))
        (self.root / 'src/main.rs').write_text('fn main() { println!("new"); }')
        self.assertEqual(self.key, cache.signature(self.root, self.environment))

    def test_corrupt_marker_is_cold(self):
        (self.root / cache.STAMP).write_text('invalid')
        self.assertFalse(cache.prepare(self.root, self.key))
        self.assertFalse((self.root / 'target').exists())

    def test_compiler_metadata_is_not_a_workspace_checkpoint(self):
        (self.root / 'target/.rustc_info.json').write_text(
            '{"rustc_fingerprint": 123, "outputs": {}}')
        self.assertFalse(cache.prepare(self.root, self.key))
        self.assertFalse((self.root / 'target').exists())

    def test_old_checkpoint_schema_is_not_reused(self):
        (self.root / 'target/.pinewasm-ci-inputs-v1.json').write_text(
            '{"version": 1, "inputs": "' + self.key + '"}')
        self.assertFalse(cache.prepare(self.root, self.key))
        self.assertFalse((self.root / 'target').exists())

    def test_symlinked_target_is_rejected_without_touching_destination(self):
        with tempfile.TemporaryDirectory() as outside:
            destination = Path(outside)
            (destination / 'keep').write_text('unrelated')
            (self.root / 'target/retained').unlink()
            (self.root / 'target').rmdir()
            (self.root / 'target').symlink_to(destination, target_is_directory=True)
            with self.assertRaisesRegex(ValueError, 'inside the dedicated checkout'):
                cache.prepare(self.root, self.key)
            with self.assertRaisesRegex(ValueError, 'inside the dedicated checkout'):
                cache.checkpoint(self.root, self.key)
            self.assertEqual((destination / 'keep').read_text(), 'unrelated')

    @unittest.skipUnless(os.environ.get('NATIVE_CACHE_CARGO_CHECK') == '1',
                         'set NATIVE_CACHE_CARGO_CHECK=1 to run the real Cargo check')
    def test_real_cargo_rechecks_changes_and_failures_in_retained_workspace(self):
        (self.root / 'Cargo.toml').write_text(
            '[package]\nname="cache-probe"\nversion="0.0.0"\nedition="2021"\n')
        (self.root / 'Cargo.lock').unlink()
        (self.root / '.cargo/config.toml').write_text('')
        (self.root / 'rust-toolchain.toml').write_text(
            '[toolchain]\nchannel="' + os.environ['PINEWASM_RUST_VERSION'] + '"\n')
        source = self.root / 'src/main.rs'
        source.write_text('fn main() { println!("before"); }\n')
        environment = {**os.environ, 'CARGO_TARGET_DIR': str(self.root / 'target'),
                       'CARGO_BUILD_JOBS': '1'}

        def run():
            return subprocess.run(['cargo', 'run', '--offline', '--quiet'],
                                  cwd=self.root, env=environment, capture_output=True, text=True)

        first = run()
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(first.stdout.strip(), 'before')
        key = cache.signature(self.root, environment)
        cache.checkpoint(self.root, key)
        self.assertTrue(cache.prepare(self.root, key))
        cache.checkpoint(self.root, key)
        time.sleep(1.1)  # Accommodate coarse source timestamp resolution.
        source.write_text('fn main() { deliberately_invalid; }\n')
        self.assertNotEqual(run().returncode, 0, 'retained output must not hide a new compile error')
        self.assertTrue(cache.prepare(self.root, key))
        cache.checkpoint(self.root, key)
        source.write_text('fn main() { println!("after"); }\n')
        second = run()
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual(second.stdout.strip(), 'after')
