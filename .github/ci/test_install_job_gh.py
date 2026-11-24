"""Pinned package admission, absolute tool resolution and job-local publishing."""

import hashlib
import importlib.util
import io
import os
from pathlib import Path
import shlex
import shutil
import stat
import subprocess
import tarfile
import tempfile
import unittest
from unittest import mock
import zipfile

SPEC = importlib.util.spec_from_file_location(
    "install_job_gh", Path(__file__).resolve().parent / "install_job_gh.py"
)
gh = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gh)


def program(version=gh.VERSION, marker=None):
    effect = (
        "" if marker is None else "printf executed > " + shlex.quote(str(marker)) + "\n"
    )
    return (
        "#!/bin/sh\n"
        + effect
        + "printf '%s\\n' 'gh version "
        + version
        + " (fixture)' '"
        + gh.RELEASE
        + "'\n"
    ).encode()


def package(asset, body, *, duplicate=False, linked=False):
    stream = io.BytesIO()
    if asset["name"].endswith(".tar.gz"):
        name = asset["name"][:-7] + "/bin/gh"
        with tarfile.open(fileobj=stream, mode="w:gz") as archive:
            for _ in range(2 if duplicate else 1):
                info = tarfile.TarInfo(name)
                info.mode = 0o755
                if linked:
                    info.type = tarfile.SYMTYPE
                    info.linkname = "/unrelated/gh"
                    archive.addfile(info)
                else:
                    info.size = len(body)
                    archive.addfile(info, io.BytesIO(body))
    else:
        name = asset["name"][:-4] + "/bin/gh"
        with zipfile.ZipFile(stream, "w") as archive:
            for _ in range(2 if duplicate else 1):
                info = zipfile.ZipInfo(name)
                info.external_attr = (
                    (stat.S_IFLNK if linked else stat.S_IFREG) | 0o755
                ) << 16
                archive.writestr(info, body)
    data = stream.getvalue()
    return data, {
        **asset,
        "bytes": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
    }


class JobGitHubCliTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name).resolve()
        self.command_file = self.root / "github-path"
        self.command_file.write_text("existing-path\n")
        self.environment = {
            **os.environ,
            "RUNNER_TEMP": str(self.root),
            "GITHUB_PATH": str(self.command_file),
        }

    def tearDown(self):
        self.temporary.cleanup()

    def run_fixture(self, platform, body=None, **options):
        data, asset = package(gh.ASSETS[platform], body or program(), **options)

        def fetch(policy, path):
            self.assertEqual(policy, asset)
            with path.open("xb") as output:
                output.write(data)

        with mock.patch.dict(gh.ASSETS, {platform: asset}):
            return gh.bootstrap(self.environment, *platform, fetch=fetch)

    def test_absolute_pinned_tool_wins_over_hostile_ambient_gh(self):
        hostile = self.root / "hostile"
        hostile.mkdir()
        ambient_marker = self.root / "ambient-was-run"
        ambient = hostile / "gh"
        ambient.write_bytes(program("0.0.0", ambient_marker))
        ambient.chmod(0o700)
        self.environment["PATH"] = (
            str(hostile) + os.pathsep + self.environment.get("PATH", "")
        )
        pinned_marker = self.root / "pinned-was-run"
        result = self.run_fixture(("Linux", "x86_64"), program(marker=pinned_marker))
        self.assertTrue(pinned_marker.exists())
        self.assertFalse(ambient_marker.exists())
        self.assertFalse(result["ambientGhUsed"])
        self.assertEqual(
            self.command_file.read_text(), "existing-path\n" + result["bin"] + "\n"
        )
        self.assertTrue(Path(result["binary"]).is_relative_to(self.root))
        next_step_environment = {
            **self.environment,
            "PATH": result["bin"] + os.pathsep + self.environment["PATH"],
        }
        self.assertEqual(
            shutil.which("gh", path=next_step_environment["PATH"]), result["binary"]
        )
        child = subprocess.run(
            ["gh", "--version"],
            env=next_step_environment,
            capture_output=True,
            text=True,
            check=True,
        )
        self.assertIn("gh version " + gh.VERSION, child.stdout)
        self.assertFalse(ambient_marker.exists())

    def test_supported_archive_formats_publish_one_job_local_binary(self):
        for platform in gh.ASSETS:
            with self.subTest(platform=platform):
                result = self.run_fixture(platform)
                self.assertEqual(result["platform"], list(platform))
                self.assertTrue(result["pathPublished"])
                self.assertFalse(result["credentialsUsedForInstall"])
                self.assertEqual(Path(result["binary"]).stat().st_mode & 0o777, 0o500)

    def test_wrong_archive_hash_never_executes_or_publishes(self):
        marker = self.root / "must-not-run"
        data, asset = package(gh.ASSETS[("Linux", "x86_64")], program(marker=marker))
        asset["sha256"] = "0" * 64
        with mock.patch.dict(gh.ASSETS, {("Linux", "x86_64"): asset}):
            with self.assertRaisesRegex(ValueError, "pinned size/hash"):
                gh.bootstrap(
                    self.environment,
                    "Linux",
                    "x86_64",
                    fetch=lambda policy, path: path.write_bytes(data),
                )
        self.assertFalse(marker.exists())
        self.assertEqual(self.command_file.read_text(), "existing-path\n")

    def test_wrong_version_cannot_fall_back_to_ambient_or_publish(self):
        with self.assertRaisesRegex(ValueError, "absolute-path version probe"):
            self.run_fixture(("Linux", "x86_64"), program("0.0.0"))
        self.assertEqual(self.command_file.read_text(), "existing-path\n")

    def test_probe_does_not_inherit_loaders_or_arbitrary_caller_credentials(self):
        marker = "MUST_NOT_REACH_VENDOR_PROBE"
        self.environment.update(
            {
                "LD_PRELOAD": marker,
                "LD_LIBRARY_PATH": marker,
                "DYLD_INSERT_LIBRARIES": marker,
                "DYLD_LIBRARY_PATH": marker,
                "GH_TOKEN": marker,
                "GITHUB_TOKEN": marker,
                "OPENROUTER_API_KEY": marker,
                "UNRELATED_SERVICE_CREDENTIAL": marker,
                "BASH_ENV": marker,
            }
        )
        observed = []

        def inspect_boundary(command, **options):
            observed.append(command)
            self.assertTrue(Path(command[0]).is_absolute())
            self.assertEqual(command[1:], ["--version"])
            environment = options["env"]
            self.assertNotIn(marker, environment.values())
            for key in (
                "LD_PRELOAD",
                "DYLD_INSERT_LIBRARIES",
                "OPENROUTER_API_KEY",
                "UNRELATED_SERVICE_CREDENTIAL",
                "BASH_ENV",
            ):
                self.assertNotIn(key, environment)
            self.assertEqual(environment["PATH"], str(Path(command[0]).parent))
            self.assertTrue(
                Path(environment["GH_CONFIG_DIR"]).is_relative_to(self.root)
            )
            self.assertTrue(
                Path(environment["XDG_CACHE_HOME"]).is_relative_to(self.root)
            )
            self.assertTrue(Path(options["cwd"]).is_relative_to(self.root))
            return subprocess.CompletedProcess(
                command,
                0,
                "gh version " + gh.VERSION + " (fixture)\n" + gh.RELEASE + "\n",
                "",
            )

        with mock.patch.object(gh.subprocess, "run", side_effect=inspect_boundary):
            result = self.run_fixture(("Linux", "x86_64"))
        self.assertEqual(len(observed), 1)
        self.assertTrue(result["pathPublished"])

    def test_duplicate_and_linked_executable_members_are_rejected(self):
        for platform in (("Linux", "x86_64"), ("Darwin", "arm64")):
            for options in ({"duplicate": True}, {"linked": True}):
                with self.subTest(
                    platform=platform, options=options
                ), self.assertRaises(ValueError):
                    self.run_fixture(platform, **options)
        self.assertEqual(self.command_file.read_text(), "existing-path\n")

    def test_source_tree_guard_is_anchored_at_the_repository_root(self):
        # RUNNER_TEMP must sit outside the whole checkout, not just .github/.
        self.assertTrue((gh.ROOT / "Cargo.toml").is_file())
        self.assertTrue((gh.ROOT / "rust-toolchain.toml").is_file())
        self.assertEqual(gh.ROOT, Path(__file__).resolve().parents[2])

    def test_missing_unsupported_or_aliased_job_context_fails_before_download(self):
        fetch = mock.Mock(side_effect=AssertionError("download must not occur"))
        with self.assertRaisesRegex(ValueError, "unsupported"):
            gh.bootstrap(self.environment, "Windows", "AMD64", fetch=fetch)
        with self.assertRaisesRegex(ValueError, "explicit job paths"):
            gh.bootstrap({}, "Linux", "x86_64", fetch=fetch)
        self.command_file.unlink()
        unrelated = self.root / "unrelated"
        unrelated.write_text("preserve")
        self.command_file.symlink_to(unrelated)
        with self.assertRaisesRegex(ValueError, "symlinks"):
            gh.bootstrap(self.environment, "Linux", "x86_64", fetch=fetch)
        self.assertEqual(unrelated.read_text(), "preserve")
        self.command_file.unlink()
        os.link(unrelated, self.command_file)
        with self.assertRaisesRegex(ValueError, "unaliased"):
            gh.bootstrap(self.environment, "Linux", "x86_64", fetch=fetch)
        self.assertEqual(unrelated.read_text(), "preserve")
        fetch.assert_not_called()


if __name__ == "__main__":
    unittest.main()
