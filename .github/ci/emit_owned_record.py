"""Write bounded CI records to the job log and a step summary.

This transport copies a CI record into the job log and step summary. It does not
sign the record or interpret the results it contains.
"""

from __future__ import annotations

import argparse
import base64
from datetime import date
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import stat
import subprocess
import sys
import tempfile


REPOSITORY = "artificial-intelligence-edge/PineWASM"
REPOSITORY_ID = "1470159731"
MARKER = "PINEWASM_OWNED_RECORD_V1"
CHUNK_BYTES = 4096
LIMITS = {"ci-record": 16384, "toolchain": 262144}
MAX_SUMMARY = 1024 * 1024
MAX_FRAME = 512 * 1024
SHA1 = re.compile(r"[0-9a-f]{40}\Z")
SHA256 = re.compile(r"[0-9a-f]{64}\Z")
POSITIVE = re.compile(r"[1-9][0-9]{0,19}\Z")
TOOLS = {"cargo", "rustc", "rustdoc"}
LANES = {
    "rust-linux-x64",
    "rust-macos-arm64",
    "rust-windows-x64",
    "msrv",
    "adversarial",
    "aot-runtime",
    "wasm",
}
CI_FIELDS = {
    "schema",
    "lane",
    "fingerprint",
    "run_id",
    "run_attempt",
    "head_sha",
    "tested_commit",
    "repository",
}
IDENTITY_FIELDS = {
    "schemaVersion",
    "kind",
    "requestedToolchain",
    "proxies",
    "parent",
    "child",
    "callerCargoConfiguration",
    "callerToolchainSelection",
    "identityVerified",
    "laterWorkflowTestOutcome",
    "selectionScope",
}


def sha256(raw):
    return hashlib.sha256(raw).hexdigest()


def encoded_json(value):
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), allow_nan=False
    ).encode("utf-8")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def text(value, maximum=512):
    return (
        isinstance(value, str)
        and 0 < len(value) <= maximum
        and not any(ord(character) < 32 or ord(character) == 127 for character in value)
    )


def digest(value):
    return isinstance(value, str) and SHA256.fullmatch(value) is not None


def revision(value):
    return isinstance(value, str) and SHA1.fullmatch(value) is not None


def git_ref(value):
    return (
        isinstance(value, str)
        and re.fullmatch(r"refs/(?:heads|tags|pull)/[A-Za-z0-9_./-]{1,480}", value)
        is not None
        and ".." not in value
        and all(
            component
            and not component.startswith(".")
            and not component.endswith((".lock", "."))
            for component in value.split("/")
        )
    )


def absolute_path(value):
    return text(value, 4096) and Path(value).is_absolute()


def keys(value, expected):
    return isinstance(value, dict) and set(value) == set(expected)


def strict_json(raw):
    def unique_pairs(items):
        result = {}
        for key, value in items:
            require(key not in result, "record JSON has duplicate keys")
            result[key] = value
        return result

    def invalid_constant(_value):
        raise ValueError("record JSON has a non-finite constant")

    try:
        return json.loads(
            raw.decode("utf-8"),
            object_pairs_hook=unique_pairs,
            parse_constant=invalid_constant,
        )
    except (UnicodeError, json.JSONDecodeError, RecursionError):
        raise ValueError("record JSON is malformed") from None


def regular_path(path):
    # Reject aliases through directory links as well as the final component.
    path = Path(path).absolute()
    require(text(str(path), 4096), "record path is invalid")
    try:
        require(path.resolve(strict=True) == path, "record path is aliased")
        metadata = path.lstat()
    except OSError:
        raise ValueError("record file is unavailable") from None
    require(
        stat.S_ISREG(metadata.st_mode) and metadata.st_nlink == 1,
        "record must be an unaliased regular file",
    )
    return path, metadata


def same_file_state(before, after):
    return all(
        getattr(before, name) == getattr(after, name)
        for name in ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns")
    )


def read_record(path, limit):
    path, before = regular_path(path)
    require(0 < before.st_size <= limit, "record size exceeds bounds")
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
    try:
        with os.fdopen(os.open(path, flags), "rb") as stream:
            opened = os.fstat(stream.fileno())
            require(
                same_file_state(before, opened)
                and stat.S_ISREG(opened.st_mode)
                and opened.st_nlink == 1,
                "record changed before reading",
            )
            raw = stream.read(limit + 1)
            after = os.fstat(stream.fileno())
    except OSError:
        raise ValueError("record could not be read") from None
    require(
        0 < len(raw) <= limit
        and len(raw) == before.st_size
        and same_file_state(before, after),
        "record changed while reading or exceeds bounds",
    )
    return raw, after


def context(environment, native_system=None, native_machine=None):
    require(environment.get("GITHUB_ACTIONS") == "true", "Actions context is required")
    require(
        environment.get("GITHUB_REPOSITORY") == REPOSITORY
        and environment.get("GITHUB_REPOSITORY_ID") == REPOSITORY_ID,
        "canonical repository context is required",
    )
    run, attempt = environment.get("GITHUB_RUN_ID"), environment.get(
        "GITHUB_RUN_ATTEMPT"
    )
    require(
        isinstance(run, str)
        and POSITIVE.fullmatch(run) is not None
        and isinstance(attempt, str)
        and POSITIVE.fullmatch(attempt) is not None,
        "run and attempt must be positive decimal identities",
    )
    job = environment.get("GITHUB_JOB")
    require(
        isinstance(job, str) and re.fullmatch(r"[A-Za-z_][A-Za-z0-9_-]{0,99}", job),
        "logical job identity is invalid",
    )
    github_sha = environment.get("GITHUB_SHA")
    workflow_sha = environment.get("GITHUB_WORKFLOW_SHA")
    candidate = environment.get("CI_HEAD_SHA")
    require(
        all(revision(value) for value in (github_sha, workflow_sha, candidate)),
        "workflow and candidate revisions must be full lowercase Git identities",
    )
    ref, workflow_ref = environment.get("GITHUB_REF"), environment.get(
        "GITHUB_WORKFLOW_REF"
    )
    require(git_ref(ref), "workflow ref is invalid")
    require(
        isinstance(workflow_ref, str)
        and re.fullmatch(
            re.escape(REPOSITORY)
            + r"/\.github/workflows/[A-Za-z0-9_-]+\.ya?ml@refs/(?:heads|tags|pull)/[A-Za-z0-9_./-]{1,480}",
            workflow_ref,
        )
        and git_ref(workflow_ref.split("@", 1)[1]),
        "canonical workflow reference is required",
    )
    event = environment.get("GITHUB_EVENT_NAME")
    require(
        event in {"push", "pull_request", "schedule", "workflow_dispatch", "workflow_call"},
        "workflow event is unsupported",
    )
    require(
        environment.get("ACTUAL_RUNNER_ENVIRONMENT") == "github-hosted",
        "actual GitHub-hosted runner context is required",
    )
    runner_os, arch, name = (
        environment.get("RUNNER_OS"),
        environment.get("RUNNER_ARCH"),
        environment.get("RUNNER_NAME"),
    )
    require(
        isinstance(name, str)
        and re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_. -]{0,127}", name)
        and name == name.strip(),
        "runner name is invalid",
    )
    observed = (
        platform.system() if native_system is None else native_system,
        platform.machine() if native_machine is None else native_machine,
    )
    platforms = {
        ("Linux", "X64"): {("Linux", "x86_64")},
        ("Linux", "ARM64"): {("Linux", "aarch64"), ("Linux", "arm64")},
        ("macOS", "ARM64"): {("Darwin", "arm64")},
    }
    require(
        observed in platforms.get((runner_os, arch), set()),
        "runner platform differs from the detected native platform",
    )
    result = {
        "repository": {"id": REPOSITORY_ID, "name": REPOSITORY},
        "invocation": {
            "runId": run,
            "runAttempt": attempt,
            "logicalJob": job,
            "githubSha": github_sha,
            "workflowSha": workflow_sha,
            "workflowRef": workflow_ref,
            "ref": ref,
            "event": event,
            "runUrl": f"https://github.com/{REPOSITORY}/actions/runs/{run}/attempts/{attempt}",
        },
        "candidateHead": candidate,
        "runner": {
            "environment": "github-hosted",
            "os": runner_os,
            "arch": arch,
            "name": name,
            "observedNativeSystem": observed[0],
            "observedNativeMachine": observed[1],
        },
    }
    if "SOURCE_REVISION" in environment:
        require(
            revision(environment["SOURCE_REVISION"]), "declared source hint is invalid"
        )
        result["declaredSourceRevisionHint"] = environment["SOURCE_REVISION"]
    return result


def observe_checkout(cwd, run=subprocess.run):
    """Read Git metadata without caller secrets, loaders, hooks or global config."""
    cwd = Path(cwd).resolve(strict=True)
    git = shutil.which("git", path="/usr/bin:/bin")
    require(git is not None, "system Git is unavailable")
    with tempfile.TemporaryDirectory(prefix="record-git-") as temporary:
        private = Path(temporary)
        environment = {
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
            "HOME": str(private),
            "XDG_CONFIG_HOME": str(private),
            "TMPDIR": str(private),
            "TMP": str(private),
            "TEMP": str(private),
            "LANG": "C",
            "LC_ALL": "C",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_TERMINAL_PROMPT": "0",
            "GIT_OPTIONAL_LOCKS": "0",
            "GIT_NO_REPLACE_OBJECTS": "1",
        }

        def query(arguments):
            try:
                response = run(
                    [
                        git,
                        "--no-optional-locks",
                        "-c",
                        "core.fsmonitor=false",
                        "-c",
                        "core.hooksPath=" + os.devnull,
                        "rev-parse",
                        *arguments,
                    ],
                    cwd=cwd,
                    env=environment,
                    stdin=subprocess.DEVNULL,
                    capture_output=True,
                    timeout=10,
                )
            except (OSError, subprocess.TimeoutExpired):
                raise ValueError("Git metadata read failed") from None
            require(
                response.returncode == 0
                and isinstance(response.stdout, bytes)
                and len(response.stdout) <= 4096
                and len(response.stderr) <= 4096,
                "Git metadata read failed",
            )
            try:
                return response.stdout.decode("ascii").strip()
            except UnicodeError:
                raise ValueError("Git metadata output is malformed") from None

        require(query(["--is-inside-work-tree"]) == "true", "Git checkout is required")
        commit = query(["--verify", "HEAD^{commit}"])
        require(revision(commit), "Git commit identity is malformed")
        tree = query(["--verify", commit + "^{tree}"])
        require(
            query(["--verify", "HEAD^{commit}"]) == commit,
            "Git checkout changed while it was read",
        )
    require(
        revision(commit) and revision(tree), "Git identities are malformed"
    )
    return {"commit": commit, "tree": tree, "worktreeCleanliness": "not asserted"}


def validate_ci(record, current, checkout):
    require(keys(record, CI_FIELDS), "ci-record schema fields differ")
    require(
        type(record["schema"]) is int and record["schema"] == 1,
        "ci-record schema differs",
    )
    require(
        isinstance(record["lane"], str) and record["lane"] in LANES,
        "ci-record lane is unsupported",
    )
    if record["lane"].startswith("rust-"):
        native = {
            "rust-linux-x64": ("Linux", "X64"),
            "rust-macos-arm64": ("macOS", "ARM64"),
            "rust-windows-x64": ("Windows", "X64"),
        }
        require(
            (current["runner"]["os"], current["runner"]["arch"])
            == native[record["lane"]],
            "ci-record native lane differs from actual runner",
        )
    require(digest(record["fingerprint"]), "ci-record fingerprint is malformed")
    require(
        record["run_id"] == current["invocation"]["runId"]
        and record["run_attempt"] == current["invocation"]["runAttempt"]
        and record["repository"] == REPOSITORY
        and record["head_sha"] == current["candidateHead"]
        and record["tested_commit"] == checkout["commit"],
        "ci-record differs from actual invocation or read checkout",
    )


def version_observation(value):
    return keys(value, {"stdout", "stderr"}) and all(
        isinstance(value[key], str) for key in ("stdout", "stderr")
    )


def validate_toolchain(record):
    # Validate the record's format without rerunning the producing tool.
    require(keys(record, IDENTITY_FIELDS), "toolchain record schema fields differ")
    require(
        type(record["schemaVersion"]) is int
        and record["schemaVersion"] == 2
        and record["kind"] == "parent-and-cargo-child-toolchain-identity"
        and record["identityVerified"] is True
        and record["laterWorkflowTestOutcome"] == "not asserted"
        and text(record["selectionScope"], 2048),
        "toolchain record kind or selection scope differs",
    )
    pin = record["requestedToolchain"]
    require(
        text(pin, 64)
        and re.fullmatch(
            r"(?:[0-9]+\.[0-9]+(?:\.[0-9]+)?|nightly-[0-9]{4}-[0-9]{2}-[0-9]{2})", pin
        ),
        "toolchain pin is mutable or malformed",
    )
    parent, child, proxies = record["parent"], record["child"], record["proxies"]
    require(
        keys(
            parent,
            {
                "pin",
                "selector",
                "toolchainRoot",
                "tools",
                "rustupVersion",
                "proxyVersions",
                "channelManifest",
            },
        )
        and parent["pin"] == pin
        and absolute_path(parent["toolchainRoot"])
        and Path(parent["toolchainRoot"]).name.startswith(pin + "-")
        and keys(parent["tools"], TOOLS)
        and version_observation(parent["rustupVersion"])
        and keys(parent["proxyVersions"], TOOLS),
        "toolchain parent schema differs",
    )
    require(
        keys(
            child,
            {
                "tools",
                "rustcSysroot",
                "invocations",
                "cargoCommand",
                "stdout",
                "stderr",
                "fixtureSha256",
            },
        )
        and keys(child["tools"], TOOLS)
        and child["rustcSysroot"] == parent["toolchainRoot"]
        and isinstance(child["invocations"], list)
        and 0 < len(child["invocations"]) <= 32
        and isinstance(child["cargoCommand"], list)
        and 0 < len(child["cargoCommand"]) <= 64
        and all(text(item, 4096) for item in child["cargoCommand"])
        and isinstance(child["stdout"], str)
        and isinstance(child["stderr"], str)
        and keys(
            child["fixtureSha256"],
            {"Cargo.toml", "Cargo.lock", "src/lib.rs", "build.rs"},
        )
        and all(digest(value) for value in child["fixtureSha256"].values()),
        "toolchain child schema differs",
    )
    require(
        keys(proxies, {"directory", "rustup", "rustupSha256", "aliases"})
        and absolute_path(proxies["directory"])
        and absolute_path(proxies["rustup"])
        and str(Path(proxies["directory"]) / "rustup") == proxies["rustup"]
        and digest(proxies["rustupSha256"])
        and keys(proxies["aliases"], TOOLS),
        "toolchain proxy schema differs",
    )
    releases = set()
    for tool in sorted(TOOLS):
        actual, observed, proxy = (
            parent["tools"][tool],
            child["tools"][tool],
            proxies["aliases"][tool],
        )
        require(
            keys(actual, {"path", "sha256", "release", "stdout", "stderr"})
            and absolute_path(actual["path"])
            and actual["path"] == str(Path(parent["toolchainRoot"]) / "bin" / tool)
            and digest(actual["sha256"])
            and text(actual["release"], 64)
            and isinstance(actual["stdout"], str)
            and isinstance(actual["stderr"], str)
            and actual["stdout"].startswith(tool + " " + actual["release"] + " ")
            and "\nrelease: " + actual["release"] in actual["stdout"],
            "toolchain parent tool schema differs",
        )
        releases.add(actual["release"])
        require(
            keys(observed, {"path", "stdout"})
            and observed["path"] == actual["path"]
            and observed["stdout"] == actual["stdout"]
            and version_observation(parent["proxyVersions"][tool])
            and parent["proxyVersions"][tool]["stdout"] == actual["stdout"],
            "toolchain child or proxy output differs",
        )
        require(
            keys(proxy, {"path", "relation", "sha256"})
            and absolute_path(proxy["path"])
            and proxy["path"] == str(Path(proxies["directory"]) / tool)
            and proxy["relation"]
            in {"same-file-rustup-alias", "byte-identical-rustup-copy"}
            and proxy["sha256"] == proxies["rustupSha256"],
            "toolchain proxy alias differs",
        )
    require(len(releases) == 1, "toolchain releases differ")
    require(
        child["cargoCommand"][:2] == [proxies["aliases"]["cargo"]["path"], "+" + pin],
        "toolchain Cargo command differs from the selected proxy and pin",
    )
    release = next(iter(releases))
    if pin.startswith("nightly-"):
        try:
            date.fromisoformat(pin[8:])
        except ValueError:
            raise ValueError("toolchain nightly date is malformed") from None
        require(
            parent["selector"] == {"kind": "dated-nightly", "date": pin[8:]}
            and release.endswith("-nightly"),
            "toolchain nightly selector differs",
        )
    else:
        expected_release = pin if pin.count(".") == 2 else pin + ".0"
        require(
            parent["selector"] == {"kind": "release", "release": expected_release}
            and release == expected_release,
            "toolchain release selector differs",
        )
    manifest = parent["channelManifest"]
    require(
        keys(manifest, {"path", "sha256", "date"})
        and absolute_path(manifest["path"])
        and manifest["path"]
        == str(
            Path(parent["toolchainRoot"])
            / "lib/rustlib/multirust-channel-manifest.toml"
        )
        and digest(manifest["sha256"])
        and text(manifest["date"], 10)
        and (not pin.startswith("nightly-") or manifest["date"] == pin[8:]),
        "toolchain installed manifest differs",
    )
    invoked = set()
    for invocation in child["invocations"]:
        require(
            keys(invocation, {"tool", "executable", "argv"})
            and isinstance(invocation["tool"], str)
            and invocation["tool"] in {"rustc", "rustdoc"}
            and invocation["executable"] == parent["tools"][invocation["tool"]]["path"]
            and isinstance(invocation["argv"], list)
            and len(invocation["argv"]) <= 256
            and all(isinstance(value, str) for value in invocation["argv"]),
            "toolchain actual child invocation differs",
        )
        invoked.add(invocation["tool"])
    require(
        invoked == {"rustc", "rustdoc"}, "toolchain actual child invocation is missing"
    )
    configurations = record["callerCargoConfiguration"]
    require(
        isinstance(configurations, list) and len(configurations) <= 128,
        "toolchain config audit differs",
    )
    for configuration in configurations:
        require(
            keys(configuration, {"path", "sha256", "keys"})
            and absolute_path(configuration["path"])
            and digest(configuration["sha256"])
            and isinstance(configuration["keys"], list)
            and all(text(key, 512) for key in configuration["keys"]),
            "toolchain config audit schema differs",
        )
    validate_caller_selection(record["callerToolchainSelection"])


def validate_caller_selection(value):
    require(
        keys(
            value,
            {
                "cwd",
                "rustupHome",
                "repositoryDefault",
                "activeToolchain",
                "defaultTools",
                "matchesRepositoryDefault",
                "forcedToolchain",
            },
        )
        and absolute_path(value["cwd"])
        and absolute_path(value["rustupHome"])
        and value["matchesRepositoryDefault"] is True
        and value["forcedToolchain"] is False
        and keys(value["defaultTools"], TOOLS),
        "toolchain caller selection audit is required",
    )
    default, active = value["repositoryDefault"], value["activeToolchain"]
    require(
        keys(
            default,
            {
                "path",
                "repositoryRoot",
                "repositoryRevision",
                "repositoryRelativePath",
                "sha256",
                "channel",
            },
        )
        and absolute_path(default["path"])
        and absolute_path(default["repositoryRoot"])
        and revision(default["repositoryRevision"])
        and digest(default["sha256"])
        and text(default["repositoryRelativePath"], 1024)
        and not Path(default["repositoryRelativePath"]).is_absolute()
        and all(
            part not in {".", "..", ""}
            for part in default["repositoryRelativePath"].split("/")
        )
        and Path(default["repositoryRelativePath"]).name == "rust-toolchain.toml"
        and str(Path(default["repositoryRoot"]) / default["repositoryRelativePath"])
        == default["path"]
        and Path(value["cwd"]).is_relative_to(default["repositoryRoot"]),
        "toolchain checked-in default binding differs",
    )
    channel = default["channel"]
    require(
        text(channel, 64)
        and re.fullmatch(
            r"(?:[0-9]+\.[0-9]+(?:\.[0-9]+)?|nightly-[0-9]{4}-[0-9]{2}-[0-9]{2})",
            channel,
        ),
        "toolchain caller default pin is mutable or malformed",
    )
    require(
        keys(active, {"name", "stdout", "stderr"})
        and text(active["name"], 160)
        and re.fullmatch(r"[A-Za-z0-9_.-]+", active["name"])
        and active["name"].startswith(channel + "-")
        and active["stdout"]
        == active["name"] + " (overridden by '" + default["path"] + "')\n"
        and isinstance(active["stderr"], str),
        "toolchain actual caller selection differs from repository default",
    )
    root = Path(value["rustupHome"]) / "toolchains" / active["name"]
    releases = set()
    for tool in sorted(TOOLS):
        observed = value["defaultTools"][tool]
        require(
            keys(observed, {"path", "release", "stdout", "stderr"})
            and observed["path"] == str(root / "bin" / tool)
            and text(observed["release"], 64)
            and isinstance(observed["stdout"], str)
            and isinstance(observed["stderr"], str)
            and observed["stdout"].startswith(tool + " " + observed["release"] + " ")
            and "\nrelease: " + observed["release"] in observed["stdout"],
            "toolchain plain caller tool output differs",
        )
        releases.add(observed["release"])
    require(len(releases) == 1, "toolchain plain caller tool releases differ")
    if channel.startswith("nightly-"):
        try:
            date.fromisoformat(channel[8:])
        except ValueError:
            raise ValueError("toolchain caller nightly date is malformed") from None
        require(
            next(iter(releases)).endswith("-nightly"),
            "toolchain caller nightly output differs",
        )
    else:
        require(
            next(iter(releases))
            == (channel if channel.count(".") == 2 else channel + ".0"),
            "toolchain caller release output differs",
        )


def frame_record(kind, raw, current, checkout, expected_sha=None):
    require(
        kind in LIMITS and 0 < len(raw) <= LIMITS[kind], "record kind or size differs"
    )
    actual_sha = sha256(raw)
    if expected_sha is not None:
        require(
            digest(expected_sha) and expected_sha == actual_sha,
            "expected record digest differs",
        )
    require(
        kind != "toolchain" or expected_sha is not None,
        "toolchain records require the expected digest",
    )
    record = strict_json(raw)
    if kind == "ci-record":
        validate_ci(record, current, checkout)
    else:
        validate_toolchain(record)
        require(
            record["callerToolchainSelection"]["repositoryDefault"][
                "repositoryRevision"
            ]
            == checkout["commit"],
            "toolchain caller checkout differs from the checkout read by the emitter",
        )
    count = (len(raw) + CHUNK_BYTES - 1) // CHUNK_BYTES
    envelope = {
        "schemaVersion": 1,
        "kind": "owned-runner-record-transport",
        "recordKind": kind,
        "origin": "owned-runner-log-and-step-summary",
        "repository": current["repository"],
        "invocation": current["invocation"],
        "runner": current["runner"],
        "source": {
            "observedCheckout": checkout,
            "candidateHead": current["candidateHead"],
            "checkoutMatchesGithubSha": checkout["commit"]
            == current["invocation"]["githubSha"],
            "checkoutMatchesCandidate": checkout["commit"] == current["candidateHead"],
        },
        "record": {"sha256": actual_sha, "bytes": len(raw)},
        "transport": {
            "encoding": "standard-base64",
            "rawChunkBytes": CHUNK_BYTES,
            "chunkCount": count,
        },
        "claims": {
            "sourceQualified": False,
            "releaseQualified": False,
            "fullLanguageConformance": False,
            "cryptographicAttestation": False,
            "githubServerArtifact": False,
            "legacyServerArtifactReuseEligible": False,
            "legacyJobLogReuseEligible": False,
            "transportAssertsTestOutcome": False,
            "laterWorkflowTestOutcome": "not asserted",
        },
    }
    if "declaredSourceRevisionHint" in current:
        envelope["source"]["declaredSourceRevisionHint"] = current[
            "declaredSourceRevisionHint"
        ]
    envelope_raw = encoded_json(envelope)
    envelope_sha = sha256(envelope_raw)
    lines = [
        MARKER
        + " HEADER "
        + encoded_json(
            {
                "envelopeSha256": envelope_sha,
                "data": base64.b64encode(envelope_raw).decode("ascii"),
            }
        ).decode("ascii")
    ]
    for index in range(count):
        chunk = raw[index * CHUNK_BYTES : (index + 1) * CHUNK_BYTES]
        value = {
            "envelopeSha256": envelope_sha,
            "index": index,
            "count": count,
            "bytes": len(chunk),
            "sha256": sha256(chunk),
            "data": base64.b64encode(chunk).decode("ascii"),
        }
        lines.append(MARKER + " CHUNK " + encoded_json(value).decode("ascii"))
    footer = {
        "envelopeSha256": envelope_sha,
        "recordSha256": actual_sha,
        "rawBytes": len(raw),
        "chunkCount": count,
    }
    lines.append(MARKER + " FOOTER " + encoded_json(footer).decode("ascii"))
    framed = ("\n".join(lines) + "\n").encode("ascii")
    require(len(framed) <= MAX_FRAME, "framed record exceeds bounds")
    summary = (
        (
            "\n### CI record\n\n"
            + "Kind: `"
            + kind
            + "`; raw bytes: `"
            + str(len(raw))
            + "`; SHA-256: `"
            + actual_sha
            + "`.\n\n"
            + "This copies the CI record below. It does not sign the record or interpret its results.\n\n```text\n"
        ).encode("ascii")
        + framed
        + b"```\n"
    )
    return framed, summary, envelope


def append_summary(path, content, input_state):
    path, before = regular_path(path)
    require(
        (before.st_dev, before.st_ino) != (input_state.st_dev, input_state.st_ino),
        "summary cannot alias the record",
    )
    require(before.st_size + len(content) <= MAX_SUMMARY, "step summary exceeds bounds")
    flags = (
        os.O_WRONLY
        | os.O_APPEND
        | getattr(os, "O_NOFOLLOW", 0)
        | getattr(os, "O_NONBLOCK", 0)
    )
    try:
        with os.fdopen(os.open(path, flags), "ab") as stream:
            opened = os.fstat(stream.fileno())
            require(
                same_file_state(before, opened)
                and stat.S_ISREG(opened.st_mode)
                and opened.st_nlink == 1,
                "step summary changed before writing",
            )
            stream.write(content)
    except OSError:
        raise ValueError("step summary could not be appended") from None


def emit(kind, path, expected_sha=None, *, environment=None, cwd=None, output=None):
    environment = dict(os.environ if environment is None else environment)
    current = context(environment)
    require(kind in LIMITS, "record kind is unsupported")
    raw, input_state = read_record(path, LIMITS[kind])
    checkout = observe_checkout(Path.cwd() if cwd is None else cwd)
    framed, summary, envelope = frame_record(kind, raw, current, checkout, expected_sha)
    summary_path = environment.get("GITHUB_STEP_SUMMARY")
    require(
        isinstance(summary_path, str) and bool(summary_path),
        "step summary path is required",
    )
    append_summary(summary_path, summary, input_state)
    (sys.stdout.buffer if output is None else output).write(framed)
    return envelope


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kind", choices=sorted(LIMITS), required=True)
    parser.add_argument("--path", type=Path, required=True)
    parser.add_argument("--expected-sha")
    args = parser.parse_args()
    try:
        emit(args.kind, args.path, args.expected_sha)
    except ValueError as error:
        # Validation errors contain fixed diagnostics, never raw input/output.
        print("CI record emission failed: " + str(error), file=sys.stderr)
        return 1
    except (OSError, TypeError, KeyError):
        print(
            "CI record emission failed: local input or output failure",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
