"""Offline tests for bounded record framing and transport."""

import base64
import copy
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location(
    "owned_record", Path(__file__).with_name("emit_owned_record.py")
)
owned = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(owned)


def hash_bytes(raw):
    return hashlib.sha256(raw).hexdigest()


def reconstruct(framed):
    """Test-only reconstruction of a framed record from its text."""
    lines = framed.decode("ascii").splitlines()
    if len(lines) < 3 or len(framed) > 512 * 1024:
        raise ValueError("incomplete or oversized frame")
    values = []
    for line in lines:
        marker, role, raw = line.split(" ", 2)
        if marker != "PINEWASM_OWNED_RECORD_V1":
            raise ValueError("foreign marker")
        values.append((role, json.loads(raw)))
    if [item[0] for item in values].count("HEADER") != 1 or values[0][0] != "HEADER":
        raise ValueError("duplicate/misplaced header")
    if [item[0] for item in values].count("FOOTER") != 1 or values[-1][0] != "FOOTER":
        raise ValueError("duplicate/misplaced footer")
    header, footer = values[0][1], values[-1][1]
    metadata = base64.b64decode(header["data"], validate=True)
    identity = hash_bytes(metadata)
    if identity != header["envelopeSha256"]:
        raise ValueError("envelope digest mismatch")
    envelope = json.loads(metadata)
    count = envelope["transport"]["chunkCount"]
    if type(count) is not int or not 1 <= count <= 64 or len(values) != count + 2:
        raise ValueError("chunk count differs")
    raw = bytearray()
    for index, (role, part) in enumerate(values[1:-1]):
        if role != "CHUNK" or part["index"] != index or part["count"] != count:
            raise ValueError("duplicate/missing/out-of-order chunk")
        chunk = base64.b64decode(part["data"], validate=True)
        if (
            part["envelopeSha256"] != identity
            or len(chunk) != part["bytes"]
            or hash_bytes(chunk) != part["sha256"]
            or not 1 <= len(chunk) <= 4096
            or (index < count - 1 and len(chunk) != 4096)
        ):
            raise ValueError("chunk digest or size differs")
        raw.extend(chunk)
    raw = bytes(raw)
    if footer != {
        "envelopeSha256": identity,
        "recordSha256": hash_bytes(raw),
        "rawBytes": len(raw),
        "chunkCount": count,
    } or envelope["record"] != {"sha256": hash_bytes(raw), "bytes": len(raw)}:
        raise ValueError("raw digest or footer differs")
    return envelope, raw


class OwnedRecordControls(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="record-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.path, self.summary = self.root / "record.json", self.root / "summary.md"
        self.summary.write_bytes(b"Earlier step summary.\n")
        self.environment = {
            "GITHUB_ACTIONS": "true",
            "GITHUB_REPOSITORY": "artificial-intelligence-edge/PineWASM",
            "GITHUB_REPOSITORY_ID": "1470159731",
            "GITHUB_RUN_ID": "40514138817",
            "GITHUB_RUN_ATTEMPT": "2",
            "GITHUB_JOB": "rust",
            "GITHUB_SHA": "a" * 40,
            "GITHUB_WORKFLOW_SHA": "b" * 40,
            "GITHUB_REF": "refs/pull/24/merge",
            "GITHUB_WORKFLOW_REF": "artificial-intelligence-edge/PineWASM/.github/workflows/ci.yml@refs/pull/24/merge",
            "GITHUB_EVENT_NAME": "pull_request",
            "CI_HEAD_SHA": "c" * 40,
            "ACTUAL_RUNNER_ENVIRONMENT": "github-hosted",
            "RUNNER_OS": "Linux",
            "RUNNER_ARCH": "X64",
            "RUNNER_NAME": "authored-control-runner",
            "GITHUB_STEP_SUMMARY": str(self.summary),
        }
        self.current = owned.context(self.environment, "Linux", "x86_64")
        self.checkout = {
            "commit": "d" * 40,
            "tree": "e" * 40,
            "worktreeCleanliness": "not asserted",
        }
        self.ci = {
            "schema": 1,
            "lane": "rust-linux-x64",
            "fingerprint": "f" * 64,
            "run_id": self.environment["GITHUB_RUN_ID"],
            "run_attempt": self.environment["GITHUB_RUN_ATTEMPT"],
            "head_sha": self.environment["CI_HEAD_SHA"],
            "tested_commit": self.checkout["commit"],
            "repository": "artificial-intelligence-edge/PineWASM",
        }

    def tool_record(self):
        root = "/installed/rustup/toolchains/1.97.1-test-host"
        tools = {}
        for tool in ("cargo", "rustc", "rustdoc"):
            tools[tool] = {
                "path": root + "/bin/" + tool,
                "sha256": "1" * 64,
                "release": "1.97.1",
                "stdout": tool + " 1.97.1 (abcdef123 2026-07-14)\nrelease: 1.97.1\n",
                "stderr": "",
            }
        return {
            "schemaVersion": 2,
            "kind": "parent-and-cargo-child-toolchain-identity",
            "requestedToolchain": "1.97.1",
            "proxies": {
                "directory": "/installed/cargo/bin",
                "rustup": "/installed/cargo/bin/rustup",
                "rustupSha256": "2" * 64,
                "aliases": {
                    tool: {
                        "path": "/installed/cargo/bin/" + tool,
                        "relation": "same-file-rustup-alias",
                        "sha256": "2" * 64,
                    }
                    for tool in tools
                },
            },
            "parent": {
                "pin": "1.97.1",
                "selector": {"kind": "release", "release": "1.97.1"},
                "toolchainRoot": root,
                "tools": tools,
                "rustupVersion": {"stdout": "rustup authored fixture\n", "stderr": ""},
                "proxyVersions": {
                    tool: {"stdout": data["stdout"], "stderr": ""}
                    for tool, data in tools.items()
                },
                "channelManifest": {
                    "path": root + "/lib/rustlib/multirust-channel-manifest.toml",
                    "sha256": "3" * 64,
                    "date": "2026-07-16",
                },
            },
            "child": {
                "tools": {
                    tool: {"path": data["path"], "stdout": data["stdout"]}
                    for tool, data in tools.items()
                },
                "rustcSysroot": root,
                "invocations": [
                    {
                        "tool": tool,
                        "executable": tools[tool]["path"],
                        "argv": ["--edition=2021"],
                    }
                    for tool in ("rustc", "rustdoc")
                ],
                "cargoCommand": [
                    "/installed/cargo/bin/cargo",
                    "+1.97.1",
                    "doc",
                    "--offline",
                    "--locked",
                    "--no-deps",
                ],
                "stdout": "",
                "stderr": "authored tiny fixture trace",
                "fixtureSha256": {
                    name: "4" * 64
                    for name in ("Cargo.toml", "Cargo.lock", "src/lib.rs", "build.rs")
                },
            },
            "callerCargoConfiguration": [
                {
                    "path": "/checkout/.cargo/config.toml",
                    "sha256": "5" * 64,
                    "keys": ["alias", "alias.xtask"],
                }
            ],
            "callerToolchainSelection": {
                "cwd": "/checkout",
                "rustupHome": "/installed/rustup",
                "repositoryDefault": {
                    "path": "/checkout/rust-toolchain.toml",
                    "repositoryRoot": "/checkout",
                    "repositoryRevision": self.checkout["commit"],
                    "repositoryRelativePath": "rust-toolchain.toml",
                    "sha256": "9" * 64,
                    "channel": "1.97.1",
                },
                "activeToolchain": {
                    "name": "1.97.1-test-host",
                    "stdout": "1.97.1-test-host (overridden by '/checkout/rust-toolchain.toml')\n",
                    "stderr": "",
                },
                "defaultTools": {
                    tool: {key: value for key, value in data.items() if key != "sha256"}
                    for tool, data in tools.items()
                },
                "matchesRepositoryDefault": True,
                "forcedToolchain": False,
            },
            "identityVerified": True,
            "laterWorkflowTestOutcome": "not asserted",
            "selectionScope": "Authored observer fixture; not later tests.",
        }

    def raw(self, record=None):
        return (
            json.dumps(self.ci if record is None else record, indent=2) + "\n \n"
        ).encode()

    def emit(self, kind="ci-record", raw=None, expected=None, environment=None):
        self.path.write_bytes(self.raw() if raw is None else raw)
        output = io.BytesIO()
        with mock.patch.object(
            owned.platform, "system", return_value="Linux"
        ), mock.patch.object(
            owned.platform, "machine", return_value="x86_64"
        ), mock.patch.object(
            owned, "observe_checkout", return_value=self.checkout
        ):
            envelope = owned.emit(
                kind,
                self.path,
                expected,
                environment=self.environment if environment is None else environment,
                cwd=self.root,
                output=output,
            )
        return output.getvalue(), envelope

    def test_ci_raw_bytes_reconstruct_exactly_in_logs_and_summary(self):
        raw = b" \n" + self.raw()
        framed, envelope = self.emit(raw=raw)
        rebuilt, actual = reconstruct(framed)
        self.assertEqual(actual, raw)
        self.assertEqual(rebuilt, envelope)
        self.assertEqual(
            envelope["record"], {"bytes": len(raw), "sha256": hash_bytes(raw)}
        )
        summary = self.summary.read_bytes()
        self.assertTrue(summary.startswith(b"Earlier step summary.\n"))
        frame_in_summary = summary.split(b"```text\n", 1)[1].rsplit(b"```\n", 1)[0]
        self.assertEqual(reconstruct(frame_in_summary), (envelope, raw))

    def test_chunk_indices_counts_and_digests_at_maximum_size(self):
        for kind, record in (("ci-record", self.ci), ("toolchain", self.tool_record())):
            with self.subTest(kind=kind):
                raw = self.raw(record)
                raw += b" " * (owned.LIMITS[kind] - len(raw))
                framed, _summary, envelope = owned.frame_record(
                    kind,
                    raw,
                    self.current,
                    self.checkout,
                    hash_bytes(raw) if kind == "toolchain" else None,
                )
                self.assertEqual(reconstruct(framed)[1], raw)
                self.assertEqual(
                    envelope["transport"]["chunkCount"], owned.LIMITS[kind] // 4096
                )
                self.assertLessEqual(len(framed), owned.MAX_FRAME)

    def test_deterministic_framing_is_stable_and_excludes_raw_bytes(self):
        first = owned.frame_record("ci-record", self.raw(), self.current, self.checkout)
        self.assertEqual(
            first,
            owned.frame_record("ci-record", self.raw(), self.current, self.checkout),
        )
        for output in first[:2]:
            self.assertNotIn(self.raw(), output)
        claims = first[2]["claims"]
        self.assertEqual(claims["laterWorkflowTestOutcome"], "not asserted")
        self.assertTrue(
            all(
                value is False
                for key, value in claims.items()
                if key != "laterWorkflowTestOutcome"
            )
        )
        self.assertNotIn("jobId", first[2]["invocation"])
        self.assertNotIn("group", first[2]["runner"])

    def test_toolchain_requires_separately_supplied_exact_digest(self):
        raw = self.raw(self.tool_record())
        for expected in (None, "0" * 64, "A" * 64):
            with self.subTest(expected=expected), self.assertRaises(ValueError):
                owned.frame_record(
                    "toolchain", raw, self.current, self.checkout, expected
                )
        framed, _summary, _envelope = owned.frame_record(
            "toolchain", raw, self.current, self.checkout, hash_bytes(raw)
        )
        self.assertEqual(reconstruct(framed)[1], raw)

    def test_legacy_tool_record_does_not_claim_the_new_caller_boundary_audit(self):
        record = self.tool_record()
        record["schemaVersion"] = 1
        record.pop("callerToolchainSelection")
        raw = self.raw(record)
        with self.assertRaises(ValueError):
            owned.frame_record(
                "toolchain", raw, self.current, self.checkout, hash_bytes(raw)
            )

    def test_missing_wrong_forced_or_different_caller_default_audit_is_rejected(self):
        variants = []
        record = self.tool_record()
        record.pop("callerToolchainSelection")
        variants.append(record)
        for key, value in (
            ("forcedToolchain", True),
            ("matchesRepositoryDefault", False),
        ):
            record = self.tool_record()
            record["callerToolchainSelection"][key] = value
            variants.append(record)
        record = self.tool_record()
        record["callerToolchainSelection"]["defaultTools"]["rustc"][
            "release"
        ] = "1.87.0"
        variants.append(record)
        record = self.tool_record()
        record["callerToolchainSelection"]["activeToolchain"][
            "stdout"
        ] = "1.97.1-test-host (directory override for 'caller')\n"
        variants.append(record)
        record = self.tool_record()
        record["callerToolchainSelection"]["repositoryDefault"][
            "repositoryRevision"
        ] = ("0" * 40)
        variants.append(record)
        for index, record in enumerate(variants):
            raw = self.raw(record)
            with self.subTest(index=index), self.assertRaises(ValueError):
                owned.frame_record(
                    "toolchain", raw, self.current, self.checkout, hash_bytes(raw)
                )

    def test_publisher_subject_builder_workflow_and_candidate_stay_distinct(self):
        environment = {**self.environment, "SOURCE_REVISION": "9" * 40}
        raw = self.raw(self.tool_record())
        _framed, envelope = self.emit("toolchain", raw, hash_bytes(raw), environment)
        self.assertEqual(envelope["source"]["candidateHead"], "c" * 40)
        self.assertEqual(envelope["source"]["observedCheckout"]["commit"], "d" * 40)
        self.assertEqual(envelope["source"]["declaredSourceRevisionHint"], "9" * 40)
        self.assertEqual(envelope["invocation"]["githubSha"], "a" * 40)
        self.assertEqual(envelope["invocation"]["workflowSha"], "b" * 40)
        self.assertFalse(envelope["source"]["checkoutMatchesCandidate"])
        self.assertFalse(envelope["source"]["checkoutMatchesGithubSha"])
        self.assertFalse(envelope["claims"]["sourceQualified"])

    def test_ci_rejects_wrong_kind_schema_or_invocation(self):
        mutations = {
            "extra": {**self.ci, "unexpected": "field"},
            "boolean-schema": {**self.ci, "schema": True},
            "future-schema": {**self.ci, "schema": 2},
            "lane": {**self.ci, "lane": "unreviewed"},
            "wrong-platform-lane": {**self.ci, "lane": "rust-macos-arm64"},
            "unavailable-windows-lane": {**self.ci, "lane": "rust-windows-x64"},
            "fingerprint": {**self.ci, "fingerprint": "key"},
            "run": {**self.ci, "run_id": "1"},
            "attempt": {**self.ci, "run_attempt": "1"},
            "candidate": {**self.ci, "head_sha": "1" * 40},
            "checkout": {**self.ci, "tested_commit": "1" * 40},
            "repository": {**self.ci, "repository": "someone/fork"},
            "wrong-kind": self.tool_record(),
        }
        for name, record in mutations.items():
            with self.subTest(name=name), self.assertRaises(ValueError):
                owned.frame_record(
                    "ci-record", self.raw(record), self.current, self.checkout
                )

    def test_tool_schema_and_actual_child_observation_are_required(self):
        mutations = []
        for key, value in (
            ("schemaVersion", True),
            ("kind", "ci-record"),
            ("identityVerified", False),
            ("laterWorkflowTestOutcome", "success"),
            ("requestedToolchain", "stable"),
        ):
            record = self.tool_record()
            record[key] = value
            mutations.append(record)
        child_path = self.tool_record()
        child_path["child"]["tools"]["rustc"]["path"] = "/brew/bin/rustc"
        mutations.append(child_path)
        child_version = self.tool_record()
        child_version["child"]["tools"]["rustdoc"]["stdout"] = "rustdoc 1.87.0\n"
        mutations.append(child_version)
        missing_doc = self.tool_record()
        missing_doc["child"]["invocations"].pop()
        mutations.append(missing_doc)
        wrong_command = self.tool_record()
        wrong_command["child"]["cargoCommand"][1] = "+stable"
        mutations.append(wrong_command)
        wrong_root = self.tool_record()
        wrong_root["parent"]["toolchainRoot"] = "/foreign/1.97.1-test-host"
        mutations.append(wrong_root)
        for index, record in enumerate(mutations):
            raw = self.raw(record)
            with self.subTest(index=index), self.assertRaises(ValueError):
                owned.frame_record(
                    "toolchain", raw, self.current, self.checkout, hash_bytes(raw)
                )

    def test_declared_msrv_and_dated_nightly_remain_distinct(self):
        for pin, release in (
            ("1.86", "1.86.0"),
            ("nightly-2026-08-15", "1.99.0-nightly"),
        ):
            record = self.tool_record()
            root = "/installed/rustup/toolchains/" + pin + "-test-host"
            record["requestedToolchain"] = record["parent"]["pin"] = pin
            record["parent"]["toolchainRoot"] = record["child"]["rustcSysroot"] = root
            record["parent"]["selector"] = (
                {"kind": "dated-nightly", "date": pin[8:]}
                if pin.startswith("nightly-")
                else {"kind": "release", "release": release}
            )
            for tool in ("cargo", "rustc", "rustdoc"):
                actual = record["parent"]["tools"][tool]
                actual["release"] = release
                actual["path"] = root + "/bin/" + tool
                actual["stdout"] = actual["stdout"].replace("1.97.1", release)
                record["child"]["tools"][tool] = {
                    "path": actual["path"],
                    "stdout": actual["stdout"],
                }
                record["parent"]["proxyVersions"][tool]["stdout"] = actual["stdout"]
            for invocation in record["child"]["invocations"]:
                invocation["executable"] = root + "/bin/" + invocation["tool"]
            record["child"]["cargoCommand"][1] = "+" + pin
            record["parent"]["channelManifest"]["path"] = (
                root + "/lib/rustlib/multirust-channel-manifest.toml"
            )
            if pin.startswith("nightly-"):
                record["parent"]["channelManifest"]["date"] = pin[8:]
            raw = self.raw(record)
            with self.subTest(pin=pin):
                framed, _summary, envelope = owned.frame_record(
                    "toolchain", raw, self.current, self.checkout, hash_bytes(raw)
                )
                self.assertEqual(
                    json.loads(reconstruct(framed)[1])["requestedToolchain"], pin
                )
                self.assertFalse(envelope["claims"]["transportAssertsTestOutcome"])

    def test_strict_json_rejects_duplicates_nonfinite_and_malformed_without_echo(self):
        for raw in (
            b'{"schema":1,"schema":1}',
            b'{"secret":"sensitive",bad}',
            b'{"schema":NaN}',
            b"\xff",
            b"[]",
        ):
            with self.subTest(raw=raw), self.assertRaises(ValueError) as caught:
                owned.frame_record("ci-record", raw, self.current, self.checkout)
            self.assertNotIn("sensitive", str(caught.exception))

    def test_context_rejects_missing_or_malformed_repository_run_and_revisions(self):
        mutations = {
            "GITHUB_ACTIONS": "false",
            "GITHUB_REPOSITORY": "someone/fork",
            "GITHUB_REPOSITORY_ID": "123",
            "GITHUB_RUN_ID": "0",
            "GITHUB_RUN_ATTEMPT": "01",
            "GITHUB_JOB": "job\ncommand",
            "GITHUB_SHA": "short",
            "GITHUB_WORKFLOW_SHA": "A" * 40,
            "CI_HEAD_SHA": "",
            "GITHUB_REF": "refs/heads/a..b",
            "GITHUB_WORKFLOW_REF": "someone/fork/.github/workflows/ci.yml@refs/heads/main",
            "GITHUB_EVENT_NAME": "pull_request_target",
            "SOURCE_REVISION": "not-a-revision",
        }
        for key, value in mutations.items():
            with self.subTest(key=key), self.assertRaises(ValueError):
                owned.context({**self.environment, key: value}, "Linux", "x86_64")
        for key in ("GITHUB_RUN_ID", "GITHUB_WORKFLOW_REF", "CI_HEAD_SHA"):
            environment = dict(self.environment)
            del environment[key]
            with self.subTest(missing=key), self.assertRaises(ValueError):
                owned.context(environment, "Linux", "x86_64")

    def test_wrong_runner_environment_platform_arch_or_name_fails(self):
        for key, value in (
            ("ACTUAL_RUNNER_ENVIRONMENT", "self-hosted"),
            ("RUNNER_OS", "Windows"),
            ("RUNNER_ARCH", "ARM64"),
            ("RUNNER_NAME", "bad\nname"),
        ):
            with self.subTest(key=key), self.assertRaises(ValueError):
                owned.context({**self.environment, key: value}, "Linux", "x86_64")
        macos = owned.context(
            {**self.environment, "RUNNER_OS": "macOS", "RUNNER_ARCH": "ARM64"},
            "Darwin",
            "arm64",
        )
        self.assertEqual(macos["runner"]["observedNativeMachine"], "arm64")

    def test_input_missing_empty_oversized_symlink_hardlink_and_fifo_fail(self):
        with self.assertRaises(ValueError):
            owned.read_record(self.path, 16384)
        for raw in (b"", b" " * 16385):
            self.path.write_bytes(raw)
            with self.assertRaises(ValueError):
                owned.read_record(self.path, 16384)
        self.path.write_bytes(self.raw())
        alias = self.root / "alias.json"
        alias.symlink_to(self.path)
        with self.assertRaises(ValueError):
            owned.read_record(alias, 16384)
        alias.unlink()
        os.link(self.path, alias)
        with self.assertRaises(ValueError):
            owned.read_record(self.path, 16384)
        alias.unlink()
        fifo = self.root / "fifo"
        os.mkfifo(fifo)
        with self.assertRaises(ValueError):
            owned.read_record(fifo, 16384)
        directory_alias = self.root / "linked-directory"
        directory_alias.symlink_to(self.root, target_is_directory=True)
        with self.assertRaises(ValueError):
            owned.read_record(directory_alias / self.path.name, 16384)

    def test_input_change_during_single_read_is_rejected(self):
        self.path.write_bytes(self.raw())
        original_fdopen = os.fdopen
        reads = []

        class ChangingReader:
            def __init__(reader, stream):
                reader.stream = stream

            def __enter__(reader):
                return reader

            def __exit__(reader, *args):
                reader.stream.close()

            def fileno(reader):
                return reader.stream.fileno()

            def read(reader, limit):
                reads.append(limit)
                raw = reader.stream.read(limit)
                self.path.write_bytes(b"replaced")
                return raw

        with mock.patch.object(
            owned.os,
            "fdopen",
            side_effect=lambda *args: ChangingReader(original_fdopen(*args)),
        ):
            with self.assertRaisesRegex(ValueError, "changed while reading"):
                owned.read_record(self.path, 16384)
        self.assertEqual(reads, [16385])

    def test_exact_single_read_snapshot_is_used_even_if_path_changes_later(self):
        raw = self.raw()
        original_read = owned.read_record
        reads = []

        def read_then_change(path, limit):
            result = original_read(path, limit)
            reads.append(result[0])
            path.write_bytes(b"not the captured buffer")
            return result

        with mock.patch.object(owned, "read_record", side_effect=read_then_change):
            framed, envelope = self.emit(raw=raw)
        self.assertEqual(reads, [raw])
        self.assertEqual(reconstruct(framed)[1], raw)
        self.assertEqual(envelope["record"]["sha256"], hash_bytes(raw))
        self.assertNotEqual(self.path.read_bytes(), raw)

    def test_summary_alias_symlink_missing_and_capacity_fail_before_log_output(self):
        raw = self.raw()
        for scenario in ("alias", "symlink", "missing", "capacity"):
            with self.subTest(scenario=scenario):
                self.path.write_bytes(raw)
                summary = self.root / (scenario + ".md")
                if scenario == "alias":
                    summary = self.path
                elif scenario == "symlink":
                    summary.symlink_to(self.summary)
                elif scenario == "capacity":
                    summary.write_bytes(b" " * owned.MAX_SUMMARY)
                environment = {**self.environment, "GITHUB_STEP_SUMMARY": str(summary)}
                output = io.BytesIO()
                with mock.patch.object(
                    owned.platform, "system", return_value="Linux"
                ), mock.patch.object(
                    owned.platform, "machine", return_value="x86_64"
                ), mock.patch.object(
                    owned, "observe_checkout", return_value=self.checkout
                ), self.assertRaises(
                    ValueError
                ):
                    owned.emit(
                        "ci-record",
                        self.path,
                        environment=environment,
                        cwd=self.root,
                        output=output,
                    )
                self.assertEqual(output.getvalue(), b"")
        self.assertEqual(self.path.read_bytes(), raw)

    def test_reconstruction_rejects_duplicate_missing_reordered_and_mutated_frames(
        self,
    ):
        framed, _summary, _envelope = owned.frame_record(
            "ci-record", self.raw() + b" " * 5000, self.current, self.checkout
        )
        lines = framed.splitlines()
        mutations = [
            lines[:1] + lines,
            lines + lines[-1:],
            lines[:2] + lines[1:],
            lines[:1] + lines[2:],
            [lines[0], lines[2], lines[1], *lines[3:]],
        ]
        chunk = json.loads(lines[1].split(b" ", 2)[2])
        chunk["sha256"] = "0" * 64
        mutations.append(
            [
                lines[0],
                b"PINEWASM_OWNED_RECORD_V1 CHUNK " + json.dumps(chunk).encode(),
                *lines[2:],
            ]
        )
        footer = json.loads(lines[-1].split(b" ", 2)[2])
        footer["recordSha256"] = "0" * 64
        mutations.append(
            [
                *lines[:-1],
                b"PINEWASM_OWNED_RECORD_V1 FOOTER " + json.dumps(footer).encode(),
            ]
        )
        for index, changed in enumerate(mutations):
            with self.subTest(index=index), self.assertRaises(ValueError):
                reconstruct(b"\n".join(changed) + b"\n")

    def test_git_exec_boundary_drops_loaders_proxies_credentials_and_caller_config(
        self,
    ):
        calls = []

        def run(argv, **kwargs):
            calls.append((argv, kwargs))
            environment = kwargs["env"]
            self.assertEqual(
                set(environment),
                {
                    "PATH",
                    "HOME",
                    "XDG_CONFIG_HOME",
                    "TMPDIR",
                    "TMP",
                    "TEMP",
                    "LANG",
                    "LC_ALL",
                    "GIT_CONFIG_NOSYSTEM",
                    "GIT_CONFIG_GLOBAL",
                    "GIT_TERMINAL_PROMPT",
                    "GIT_OPTIONAL_LOCKS",
                    "GIT_NO_REPLACE_OBJECTS",
                },
            )
            self.assertEqual(environment["GIT_CONFIG_GLOBAL"], os.devnull)
            self.assertNotEqual(environment["HOME"], str(self.root))
            self.assertEqual(kwargs["cwd"], self.root)
            self.assertEqual(kwargs["stdin"], subprocess.DEVNULL)
            self.assertTrue(Path(argv[0]).is_absolute())
            result = (
                b"true\n"
                if argv[-1] == "--is-inside-work-tree"
                else (
                    ("d" if argv[-1] == "HEAD^{commit}" else "e") * 40 + "\n"
                ).encode()
            )
            return subprocess.CompletedProcess(argv, 0, stdout=result, stderr=b"")

        hostile = {
            "HOME": str(self.root),
            "LD_PRELOAD": "hostile.so",
            "DYLD_INSERT_LIBRARIES": "hostile.dylib",
            "HTTPS_PROXY": "https://proxy.invalid",
            "ARBITRARY_PROVIDER_SECRET": "sensitive",
            "GH_TOKEN": "sensitive",
            "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": "alias.rev-parse",
            "GIT_CONFIG_VALUE_0": "!hostile",
        }
        with mock.patch.dict(os.environ, hostile):
            checkout = owned.observe_checkout(self.root, run=run)
        self.assertEqual(checkout, self.checkout)
        self.assertEqual(len(calls), 4)
        self.assertIn("d" * 40 + "^{tree}", calls[2][0])

    def test_git_checkout_race_rejects_inconsistent_head(self):
        outputs = iter(
            (
                b"true\n",
                ("d" * 40 + "\n").encode(),
                ("e" * 40 + "\n").encode(),
                ("f" * 40 + "\n").encode(),
            )
        )

        def run(argv, **kwargs):
            return subprocess.CompletedProcess(
                argv, 0, stdout=next(outputs), stderr=b""
            )

        with self.assertRaisesRegex(ValueError, "checkout changed"):
            owned.observe_checkout(self.root, run=run)

    def test_git_failure_and_malformed_metadata_never_echo_output(self):
        for result in (
            subprocess.CompletedProcess([], 1, stdout=b"", stderr=b"sensitive"),
            subprocess.CompletedProcess([], 0, stdout=b"sensitive", stderr=b""),
            subprocess.CompletedProcess([], 0, stdout=b" " * 4097, stderr=b""),
        ):
            with self.subTest(result=result.returncode), self.assertRaises(
                ValueError
            ) as caught:
                owned.observe_checkout(self.root, run=lambda *args, **kwargs: result)
            self.assertNotIn("sensitive", str(caught.exception))

    def test_wrong_context_or_malformed_input_never_emits_raw(self):
        for environment, raw in (
            ({**self.environment, "GITHUB_REPOSITORY_ID": "1"}, self.raw()),
            (self.environment, b'{"credential":"sensitive",bad}'),
        ):
            self.path.write_bytes(raw)
            output = io.BytesIO()
            before = self.summary.read_bytes()
            with mock.patch.object(
                owned.platform, "system", return_value="Linux"
            ), mock.patch.object(
                owned.platform, "machine", return_value="x86_64"
            ), mock.patch.object(
                owned, "observe_checkout", return_value=self.checkout
            ), self.assertRaises(
                ValueError
            ) as caught:
                owned.emit(
                    "ci-record",
                    self.path,
                    environment=environment,
                    cwd=self.root,
                    output=output,
                )
            self.assertNotIn("sensitive", str(caught.exception))
            self.assertEqual(output.getvalue(), b"")
            self.assertEqual(self.summary.read_bytes(), before)

    def test_real_cli_uses_test_owned_git_checkout_and_keeps_failures_bounded(self):
        # Local temporary fixture, deleted on exit.
        git = "/usr/bin/git"
        private = self.root / "cli-home"
        private.mkdir()
        clean = {
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
            "HOME": str(private),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "LANG": "C",
            "LC_ALL": "C",
            "TMPDIR": str(private),
        }

        def git_command(*arguments):
            return (
                subprocess.run(
                    [git, *arguments],
                    cwd=self.root,
                    env=clean,
                    stdin=subprocess.DEVNULL,
                    capture_output=True,
                    check=True,
                    timeout=10,
                )
                .stdout.strip()
                .decode("ascii")
            )

        git_command("init", "--quiet", "--initial-branch=main")
        git_command(
            "-c",
            "user.name=Authored transport control",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--allow-empty",
            "--no-verify",
            "-m",
            "Authored local fixture",
        )
        head = git_command("rev-parse", "HEAD")
        native = (owned.platform.system(), owned.platform.machine())
        platform_context = {
            ("Darwin", "arm64"): ("macOS", "ARM64"),
            ("Linux", "aarch64"): ("Linux", "ARM64"),
            ("Linux", "arm64"): ("Linux", "ARM64"),
            ("Linux", "x86_64"): ("Linux", "X64"),
        }[native]
        environment = {
            **clean,
            **self.environment,
            "GITHUB_RUN_ID": "1",
            "GITHUB_RUN_ATTEMPT": "1",
            "GITHUB_JOB": "msrv",
            "GITHUB_SHA": head,
            "GITHUB_WORKFLOW_SHA": head,
            "CI_HEAD_SHA": head,
            "RUNNER_OS": platform_context[0],
            "RUNNER_ARCH": platform_context[1],
        }
        raw = self.raw(
            {
                **self.ci,
                "lane": "msrv",
                "run_id": "1",
                "run_attempt": "1",
                "head_sha": head,
                "tested_commit": head,
            }
        )
        self.path.write_bytes(raw)
        command = [
            sys.executable,
            "-B",
            str(Path(owned.__file__).resolve()),
            "--kind",
            "ci-record",
            "--path",
            str(self.path),
        ]
        result = subprocess.run(
            command,
            cwd=self.root,
            env=environment,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            timeout=15,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        envelope, recovered = reconstruct(result.stdout)
        self.assertEqual(recovered, raw)
        self.assertEqual(envelope["source"]["observedCheckout"]["commit"], head)
        before = self.summary.read_bytes()
        self.path.write_bytes(b'{"credential":"sensitive",bad}')
        result = subprocess.run(
            command,
            cwd=self.root,
            env=environment,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            timeout=15,
        )
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, b"")
        self.assertNotIn(b"sensitive", result.stderr)
        self.assertNotIn(b"Traceback", result.stderr)
        self.assertEqual(self.summary.read_bytes(), before)


if __name__ == "__main__":
    unittest.main()
