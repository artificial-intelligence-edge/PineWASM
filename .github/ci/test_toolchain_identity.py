"""Identity checks use local test tool fixtures; no installs or network."""

import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock
import subprocess


SPEC = importlib.util.spec_from_file_location(
    "identity", Path(__file__).with_name("toolchain_identity.py")
)
identity = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(identity)


class ToolchainIdentityControls(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="toolchain-check-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.proxies = self.root / ".cargo/bin"
        self.proxies.mkdir(parents=True)
        rustup = self.proxies / "rustup"
        rustup.write_bytes(b"local rustup fixture")
        rustup.chmod(0o700)
        for tool in identity.TOOLS:
            (self.proxies / tool).symlink_to("rustup")
        self.brew = self.root / "brew/bin"
        self.brew.mkdir(parents=True)
        for tool in identity.TOOLS:
            path = self.brew / tool
            path.write_bytes(b"stale 1.87 toolchain fixture")
            path.chmod(0o700)
        self.environment = {
            "HOME": str(self.root),
            "CARGO_HOME": str(self.root / ".cargo"),
            "RUSTUP_HOME": str(self.root / ".rustup"),
            "PATH": str(self.brew) + os.pathsep + str(self.proxies),
        }
        self.calls = []
        self.install_fixture("1.97.1", "1.97.1", "2026-07-16")
        self.default_tools, self.default_versions = dict(self.tools), dict(
            self.versions
        )
        self.caller = self.root / "caller"
        self.caller.mkdir()
        self.default_path = self.caller / "rust-toolchain.toml"
        self.default_path.write_text('[toolchain]\nchannel="1.97.1"\n')
        self.tracked = {"rust-toolchain.toml": self.default_path.read_text()}
        self.revision = "a" * 40
        self.active_stdout = (
            self.default_tools["cargo"].parent.parent.name
            + " (overridden by '"
            + str(self.default_path)
            + "')\n"
        )

    def install_fixture(self, pin, release, date):
        self.pin = pin
        self.toolchain = self.root / ".rustup/toolchains" / (pin + "-test-host")
        (self.toolchain / "bin").mkdir(parents=True, exist_ok=True)
        self.tools = {}
        self.versions = {}
        for tool in identity.TOOLS:
            path = self.toolchain / "bin" / tool
            path.write_bytes(("local " + tool + " " + release).encode())
            path.chmod(0o700)
            self.tools[tool] = path
            self.versions[tool] = (
                tool
                + " "
                + release
                + " (abcdef123 2026-07-14)\nrelease: "
                + release
                + "\n"
            )
        manifest = self.toolchain / "lib/rustlib/multirust-channel-manifest.toml"
        manifest.parent.mkdir(parents=True, exist_ok=True)
        manifest.write_text(
            'date="'
            + date
            + '"\n[pkg.rustc]\nversion="'
            + release
            + ' (abcdef123 2026-07-14)"\n[pkg.cargo]\nversion="0.0.0 (abcdef123 2026-07-14)"\n'
        )

    def fake_run(self, argv, *, environment, cwd=None):
        self.calls.append((argv, dict(environment)))
        if Path(argv[0]).name == "git":
            if argv[-2:] == ["rev-parse", "--show-toplevel"]:
                output = str(self.caller) + "\n"
            elif argv[-2:] == ["--verify", "HEAD^{commit}"]:
                output = self.revision + "\n"
            elif argv[-2] == "show" and argv[-1].startswith(self.revision + ":"):
                name = argv[-1].split(":", 1)[1]
                if name not in self.tracked:
                    raise ValueError("untracked test default")
                output = self.tracked[name]
            else:
                raise AssertionError(argv)
        elif argv[1:] == ["show", "active-toolchain"]:
            output = self.active_stdout
        elif argv[1:] == ["--version"]:
            output = "rustup 1.28.2 (local fixture)\n"
        elif argv[1:2] == ["which"] and len(argv) == 3:
            output = str(self.default_tools[argv[2]]) + "\n"
        elif argv[1:4] == ["which", "--toolchain", self.pin]:
            output = str(self.tools[argv[4]]) + "\n"
        elif argv[1:] == ["--print", "sysroot"]:
            output = str(self.toolchain) + "\n"
        elif argv[1:] == ["--version", "--verbose"]:
            versions = (
                self.versions
                if "RUSTUP_TOOLCHAIN" in environment
                else self.default_versions
            )
            output = versions[Path(argv[0]).name]
        elif argv[1:] == ["+" + self.pin, "--version", "--verbose"]:
            output = self.versions[Path(argv[0]).name]
        else:
            raise AssertionError(argv)
        return {"stdout": output, "stderr": ""}

    def parent(self):
        proxies = identity.discover_proxies(self.environment)
        selected = identity.limited_environment(
            self.environment, proxies["directory"], self.pin, self.root / "private"
        )
        parent, selected = identity.parent_identity(
            self.pin,
            self.environment,
            proxies,
            self.fake_run,
            execution_environment=selected,
        )
        return parent, selected, proxies

    def child(self, parent):
        return {
            "tools": {
                name: {"path": tool["path"], "stdout": tool["stdout"]}
                for name, tool in parent["tools"].items()
            },
            "rustcSysroot": str(self.toolchain),
            "invocations": [
                {"tool": tool, "executable": str(self.tools[tool]), "argv": []}
                for tool in ("rustc", "rustdoc")
            ],
        }

    def test_homebrew_shadow_is_replaced_by_verified_proxy_directory(self):
        parent, selected, proxies = self.parent()
        self.assertEqual(selected["PATH"].split(os.pathsep)[0], str(self.proxies))
        self.assertEqual(proxies["directory"], str(self.proxies))
        self.assertEqual({v["release"] for v in parent["tools"].values()}, {"1.97.1"})
        self.assertFalse(any(str(self.brew) in call[0][0] for call in self.calls))

    def test_missing_and_foreign_proxy_tools_are_rejected(self):
        (self.proxies / "rustdoc").unlink()
        with self.assertRaisesRegex(ValueError, "missing rustup proxy"):
            identity.verify_proxies(self.proxies)
        (self.proxies / "rustdoc").write_bytes(b"foreign rustdoc")
        (self.proxies / "rustdoc").chmod(0o700)
        with self.assertRaisesRegex(ValueError, "foreign executable"):
            identity.verify_proxies(self.proxies)

    def test_symlinked_manager_discovers_its_verified_installation(self):
        installation = self.root / "cellar/rustup/bin"
        installation.mkdir(parents=True)
        (installation / "rustup").write_bytes(b"local rustup fixture")
        (installation / "rustup").chmod(0o700)
        for tool in identity.TOOLS:
            (installation / tool).symlink_to("rustup")
        (self.brew / "rustup").symlink_to(installation / "rustup")
        environment = dict(self.environment)
        environment["HOME"] = str(self.root / "other-home")
        environment.pop("CARGO_HOME")
        environment["PATH"] = str(self.brew)
        self.assertEqual(
            identity.discover_proxies(environment)["directory"], str(installation)
        )

    def test_homebrew_wrappers_discover_the_verified_libexec_proxies(self):
        keg = self.root / "cellar/rustup"
        installation = keg / "libexec/bin"
        installation.mkdir(parents=True)
        (installation / "rustup").write_bytes(b"local rustup fixture")
        (installation / "rustup").chmod(0o700)
        (keg / "bin").mkdir()
        for tool in ("rustup", *identity.TOOLS):
            (keg / "bin" / tool).write_text("#!/bin/sh\nexec ../libexec/bin/" + tool)
            (keg / "bin" / tool).chmod(0o700)
        for tool in identity.TOOLS:
            (installation / tool).symlink_to("rustup")
        (self.brew / "rustup").symlink_to(keg / "bin/rustup")
        environment = dict(self.environment)
        environment["HOME"] = str(self.root / "other-home")
        environment.pop("CARGO_HOME")
        environment["PATH"] = str(self.brew)
        self.assertEqual(
            identity.discover_proxies(environment)["directory"], str(installation)
        )

    def test_byte_identical_official_proxy_copy_is_supported(self):
        (self.proxies / "cargo").unlink()
        (self.proxies / "cargo").write_bytes((self.proxies / "rustup").read_bytes())
        (self.proxies / "cargo").chmod(0o700)
        self.assertEqual(
            identity.verify_proxies(self.proxies)["aliases"]["cargo"]["relation"],
            "byte-identical-rustup-copy",
        )

    def test_selected_missing_or_foreign_managed_tool_fails(self):
        self.tools["rustdoc"].unlink()
        with self.assertRaisesRegex(ValueError, "missing/nonabsolute"):
            self.parent()
        self.tools["rustdoc"] = self.brew / "rustdoc"
        with self.assertRaisesRegex(ValueError, "outside the pinned"):
            self.parent()

    def test_requested_version_is_never_substituted_for_observation(self):
        self.versions["rustc"] = "rustc 1.87.0 (old)\nrelease: 1.87.0\n"
        with self.assertRaisesRegex(ValueError, "observed tool version"):
            self.parent()

    def test_same_release_cargo_from_another_build_is_not_the_pinned_manifest(self):
        self.versions["cargo"] = (
            "cargo 1.97.1 (different123 2026-07-14)\nrelease: 1.97.1\n"
        )
        with self.assertRaisesRegex(
            ValueError, "Cargo differs from installed channel manifest"
        ):
            self.parent()

    def test_wrappers_and_compiler_overrides_fail_before_execution(self):
        for key in sorted(identity.OVERRIDES):
            with self.subTest(key=key):
                with self.assertRaisesRegex(ValueError, "inherited tool override"):
                    identity.observe(
                        self.pin, {**self.environment, key: "untrusted"}, self.fake_run
                    )
        self.assertEqual(self.calls, [])

    def test_inherited_rustup_toolchain_cannot_hide_behind_isolated_requested_pin(self):
        for value in ("1.87.0", ""):
            with self.subTest(value=value), self.assertRaisesRegex(
                ValueError, "RUSTUP_TOOLCHAIN"
            ):
                identity.observe(
                    "1.97.1",
                    {**self.environment, "RUSTUP_TOOLCHAIN": value},
                    self.fake_run,
                    cwd=self.caller,
                )
        self.assertEqual(self.calls, [])

    def caller_selection(self):
        proxies = identity.discover_proxies(self.environment)
        selected = identity.limited_environment(
            self.environment,
            proxies["directory"],
            self.pin,
            self.root / "selection-private",
        )
        return identity.caller_selection(self.caller, selected, proxies, self.fake_run)

    def test_actual_plain_default_is_bound_to_checked_in_source_without_forced_pin(
        self,
    ):
        result = self.caller_selection()
        self.assertEqual(result["repositoryDefault"]["channel"], "1.97.1")
        self.assertEqual(
            result["repositoryDefault"]["repositoryRevision"], self.revision
        )
        self.assertEqual(
            result["repositoryDefault"]["sha256"],
            hashlib.sha256(self.default_path.read_bytes()).hexdigest(),
        )
        self.assertTrue(result["matchesRepositoryDefault"])
        self.assertFalse(result["forcedToolchain"])
        self.assertTrue(
            all(
                "RUSTUP_TOOLCHAIN" not in environment
                for _argv, environment in self.calls
            )
        )
        self.assertTrue(
            all(
                value["release"] == "1.97.1"
                for value in result["defaultTools"].values()
            )
        )

    def test_wrong_or_even_same_pin_directory_override_cannot_replace_default_file(
        self,
    ):
        for actual in (
            "1.87.0-test-host (directory override for 'caller')\n",
            "1.97.1-test-host (directory override for 'caller')\n",
            "1.97.1-test-host (default)\n",
        ):
            self.active_stdout = actual
            with self.subTest(actual=actual), self.assertRaisesRegex(
                ValueError, "actual caller Rustup selection"
            ):
                self.caller_selection()

    def test_default_file_must_be_tracked_unaliased_and_immutable(self):
        original = self.default_path.read_bytes()
        cases = [
            b'[toolchain]\nchannel="1.87.0"\n',
            b'[toolchain]\nchannel="stable"\n',
            b'[toolchain]\npath="DO_NOT_ECHO"\n',
            b'broken="DO_NOT_ECHO',
        ]
        for raw in cases:
            self.default_path.write_bytes(raw)
            with self.subTest(raw=raw), self.assertRaises(ValueError) as error:
                self.caller_selection()
            self.assertNotIn("DO_NOT_ECHO", str(error.exception))
        self.default_path.write_bytes(original)
        destination = self.caller / "default-source"
        destination.write_bytes(original)
        self.default_path.unlink()
        self.default_path.symlink_to(destination)
        with self.assertRaisesRegex(ValueError, "aliased"):
            self.caller_selection()

    def test_explicit_msrv_and_source_safety_nightly_do_not_replace_plain_default(self):
        for pin, release, date in [
            ("1.86", "1.86.0", "2025-01-01"),
            ("nightly-2026-08-15", "1.99.0-nightly", "2026-08-15"),
        ]:
            self.install_fixture(pin, release, date)
            result = self.caller_selection()
            self.assertEqual(result["repositoryDefault"]["channel"], "1.97.1")
            self.assertEqual(
                {tool["release"] for tool in result["defaultTools"].values()},
                {"1.97.1"},
            )
            parent, _selected, _proxies = self.parent()
            self.assertEqual(
                {tool["release"] for tool in parent["tools"].values()}, {release}
            )

    def test_nearest_nested_default_must_itself_be_checked_in(self):
        nested = self.caller / "nested"
        nested.mkdir()
        path = nested / "rust-toolchain.toml"
        path.write_bytes(self.default_path.read_bytes())
        proxies = identity.discover_proxies(self.environment)
        selected = identity.limited_environment(
            self.environment,
            proxies["directory"],
            self.pin,
            self.root / "nested-private",
        )
        with self.assertRaisesRegex(ValueError, "Git binding failed"):
            identity.caller_selection(nested, selected, proxies, self.fake_run)
        self.tracked["nested/rust-toolchain.toml"] = path.read_text()
        self.active_stdout = (
            self.default_tools["cargo"].parent.parent.name
            + " (overridden by '"
            + str(path)
            + "')\n"
        )
        result = identity.caller_selection(nested, selected, proxies, self.fake_run)
        self.assertEqual(
            result["repositoryDefault"]["repositoryRelativePath"],
            "nested/rust-toolchain.toml",
        )

    def test_wrong_plain_default_compiler_or_path_fails_independently_of_explicit_pin(
        self,
    ):
        self.default_versions["rustc"] = "rustc 1.87.0 (old)\nrelease: 1.87.0\n"
        with self.assertRaisesRegex(ValueError, "observed tool version"):
            self.caller_selection()
        self.default_versions["rustc"] = self.versions["rustc"]
        self.default_tools["rustdoc"] = self.brew / "rustdoc"
        with self.assertRaisesRegex(ValueError, "plain caller tool resolution"):
            self.caller_selection()

    def test_stable_msrv_and_dated_nightly_are_distinct(self):
        self.assertEqual(
            identity.pin_kind("1.86"), {"kind": "release", "release": "1.86.0"}
        )
        self.install_fixture("1.86", "1.86.0", "2025-04-03")
        self.assertEqual(self.parent()[0]["tools"]["rustc"]["release"], "1.86.0")
        self.install_fixture("nightly-2026-08-15", "1.99.0-nightly", "2026-08-15")
        parent, selected, _ = self.parent()
        self.assertEqual(parent["tools"]["rustc"]["release"], "1.99.0-nightly")
        self.assertEqual(selected["RUSTUP_TOOLCHAIN"], "nightly-2026-08-15")
        self.assertNotIn("RUSTC", selected)
        self.assertNotIn("RUSTDOC", selected)

    def test_wrong_nightly_date_and_mutable_aliases_fail(self):
        self.install_fixture("nightly-2026-08-15", "1.99.0-nightly", "2026-08-14")
        with self.assertRaisesRegex(ValueError, "manifest date"):
            self.parent()
        for pin in (
            "stable",
            "nightly",
            "beta",
            "1.97.1\nvalue=x",
            "nightly-2026-99-17",
        ):
            with self.subTest(pin=pin), self.assertRaises(ValueError):
                identity.pin_kind(pin)

    def test_wrong_child_compiler_or_rustdoc_path_fails(self):
        parent, _, _ = self.parent()
        for tool in identity.TOOLS:
            child = self.child(parent)
            child["tools"][tool]["path"] = str(self.brew / tool)
            with self.subTest(tool=tool), self.assertRaisesRegex(
                ValueError, "Cargo child selected"
            ):
                identity.validate_child(parent, child)

    def test_wrong_child_version_or_actual_doc_command_fails(self):
        parent, _, _ = self.parent()
        child = self.child(parent)
        child["tools"]["rustc"]["stdout"] = "rustc 1.87.0 (old)"
        with self.assertRaisesRegex(ValueError, "different version"):
            identity.validate_child(parent, child)
        child = self.child(parent)
        child["invocations"] = [
            item for item in child["invocations"] if item["tool"] != "rustdoc"
        ]
        with self.assertRaisesRegex(ValueError, "actual Cargo invocation"):
            identity.validate_child(parent, child)

    def test_verbose_command_reader_ignores_environment_prefixes(self):
        commands = identity.cargo_commands(
            " Running `CARGO=/official/cargo VALUE='contains spaces' /official/rustdoc --edition=2021 src/lib.rs`\n"
        )
        self.assertEqual(commands[0]["executable"], "/official/rustdoc")
        self.assertEqual(commands[0]["tool"], "rustdoc")

    def test_record_and_environment_outputs_never_export_global_compilers(self):
        parent, _, proxies = self.parent()
        record = {"parent": parent, "proxies": proxies, "child": self.child(parent)}
        target, github_path, github_output = [
            self.root / name
            for name in ["identity.json", "github-path", "github-output"]
        ]
        outputs = identity.write_record(record, target, github_path, github_output)
        self.assertEqual(github_path.read_text(), str(self.proxies) + "\n")
        self.assertEqual(
            outputs["record_sha256"], hashlib.sha256(target.read_bytes()).hexdigest()
        )
        self.assertNotIn("RUSTC=", github_output.read_text())
        self.assertNotIn("RUSTDOC=", github_output.read_text())
        self.assertNotIn("RUSTUP_TOOLCHAIN=", github_output.read_text())
        with self.assertRaisesRegex(ValueError, "new evidence path"):
            identity.write_record(record, target, github_path)

    def test_path_injection_aliasing_and_oversized_records_fail(self):
        parent, _, proxies = self.parent()
        record = {"parent": parent, "proxies": proxies}
        with self.assertRaisesRegex(ValueError, "unsafe"):
            identity.write_record(
                record, self.root / "record\nRUSTC=foreign", self.root / "path"
            )
        with self.assertRaisesRegex(ValueError, "distinct"):
            identity.write_record(record, self.root / "same", self.root / "same")
        with self.assertRaisesRegex(ValueError, "exceeds bounds"):
            identity.write_record(
                {**record, "data": "x" * identity.MAX_RECORD},
                self.root / "large",
                self.root / "path",
            )

    def test_real_caller_config_audit_allows_aliases_and_records_no_values(self):
        workspace = self.root / "workspace/nested"
        workspace.mkdir(parents=True)
        cargo = self.root / "workspace/.cargo"
        cargo.mkdir()
        config = cargo / "config.toml"
        config.write_text('[alias]\nxtask="run --locked -p pinewasm_xtask --"\n')
        rows = identity.audit_cargo_configs(self.environment, workspace)
        self.assertEqual(
            rows,
            [
                {
                    "path": str(config.resolve()),
                    "sha256": hashlib.sha256(config.read_bytes()).hexdigest(),
                    "keys": ["alias", "alias.xtask"],
                }
            ],
        )
        self.assertNotIn("run --locked", json.dumps(rows))

    def test_compiler_config_env_includes_targets_and_runners_fail_closed(self):
        workspace = self.root / "workspace"
        workspace.mkdir()
        config = self.root / ".cargo/config.toml"
        cases = [
            '[build]\nrustc="DO_NOT_PRINT_COMPILER_VALUE"\n',
            '[build]\nrustdoc="DO_NOT_PRINT_COMPILER_VALUE"\n',
            '[build]\nrustc-wrapper="DO_NOT_PRINT_COMPILER_VALUE"\n',
            '[build]\ntarget="wasm32-unknown-unknown"\n',
            '[target."cfg(unix)"]\nrunner="DO_NOT_PRINT_COMPILER_VALUE"\n',
            '[env]\nRUSTC={value="DO_NOT_PRINT_COMPILER_VALUE",force=true}\n',
            '[env]\nPATH={value="DO_NOT_PRINT_COMPILER_VALUE",force=true}\n',
            '[env]\nLD_PRELOAD={value="DO_NOT_PRINT_COMPILER_VALUE",force=true}\n',
            'include="DO_NOT_PRINT_COMPILER_VALUE"\n',
        ]
        for text in cases:
            config.write_text(text)
            with self.subTest(text=text):
                with self.assertRaises(ValueError) as error:
                    identity.audit_cargo_configs(self.environment, workspace)
                self.assertNotIn("DO_NOT_PRINT_COMPILER_VALUE", str(error.exception))
        config.unlink()
        with self.assertRaisesRegex(ValueError, "inherited tool override"):
            identity.observe(
                self.pin,
                {**self.environment, "CARGO_TARGET_TEST_RUNNER": "foreign"},
                self.fake_run,
                cwd=workspace,
            )

    def test_config_symlinks_oversize_and_parse_errors_are_not_interpreted(self):
        workspace = self.root / "workspace"
        workspace.mkdir()
        config = self.root / ".cargo/config.toml"
        for raw in [
            b'broken="DO_NOT_PRINT_PARSE_EXCERPT',
            b"x" * (identity.MAX_OUTPUT + 1),
        ]:
            config.write_bytes(raw)
            with self.assertRaises(ValueError) as error:
                identity.audit_cargo_configs(self.environment, workspace)
            self.assertNotIn("DO_NOT_PRINT_PARSE_EXCERPT", str(error.exception))
        config.unlink()
        destination = self.root / "configuration"
        destination.write_text('[alias]\nxtask="safe"\n')
        config.symlink_to(destination)
        with self.assertRaisesRegex(ValueError, "symlink"):
            identity.audit_cargo_configs(self.environment, workspace)

    def test_loader_and_arbitrary_credentials_never_cross_exec_boundary(self):
        caller = {
            **self.environment,
            "DYLD_INSERT_LIBRARIES": "HOSTILE_LOADER",
            "LD_PRELOAD": "HOSTILE_LOADER",
            "HTTP_PROXY": "PROVIDER_SECRET",
            "UNRELATED_PROVIDER_CREDENTIAL": "PROVIDER_SECRET",
        }
        workspace = self.caller
        boundaries = []

        def execute(argv, *, cwd, env, text, capture_output, timeout):
            boundaries.append(dict(env))
            for name in [
                "DYLD_INSERT_LIBRARIES",
                "LD_PRELOAD",
                "HTTP_PROXY",
                "UNRELATED_PROVIDER_CREDENTIAL",
            ]:
                self.assertNotIn(name, env)
            self.assertNotEqual(env["HOME"], caller["HOME"])
            self.assertNotEqual(env["CARGO_HOME"], caller["CARGO_HOME"])
            self.assertEqual(env["RUSTUP_HOME"], str((self.root / ".rustup").resolve()))
            self.assertEqual(
                env["PATH"].split(os.pathsep),
                [str(self.proxies), "/usr/bin", "/bin", "/usr/sbin", "/sbin"],
            )
            if "RUSTUP_TOOLCHAIN" in env:
                self.assertNotEqual(Path(cwd), workspace)
            else:
                self.assertEqual(Path(cwd), workspace)
            if "doc" in argv:
                observed = Path(env["IDENTITY_PROBE_DIR"])
                for name in identity.TOOLS:
                    (observed / (name + ".path")).write_text(str(self.tools[name]))
                    (observed / (name + ".version")).write_text(self.versions[name])
                (observed / "rustc.sysroot").write_text(str(self.toolchain))
                stderr = "\n".join(
                    " Running `" + str(self.tools[name]) + " fixture`"
                    for name in ["rustc", "rustdoc"]
                )
                return subprocess.CompletedProcess(argv, 0, "", stderr)
            response = self.fake_run(argv, environment=env, cwd=cwd)
            return subprocess.CompletedProcess(
                argv, 0, response["stdout"], response["stderr"]
            )

        with mock.patch.object(identity.subprocess, "run", execute):
            record = identity.observe(
                self.pin, caller, run=identity.command, cwd=workspace
            )
        self.assertTrue(record["identityVerified"])
        self.assertGreater(len(boundaries), 8)
        self.assertNotIn("HOSTILE_LOADER", json.dumps(record))
        self.assertNotIn("PROVIDER_SECRET", json.dumps(record))


if __name__ == "__main__":
    unittest.main()
