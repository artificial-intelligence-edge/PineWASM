"""Check rustup-selected parent and Cargo child tools without changing defaults."""

from __future__ import annotations

import argparse
from datetime import date
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import stat
import subprocess
import tempfile
import tomllib


TOOLS = ("cargo", "rustc", "rustdoc")
MAX_OUTPUT = 65536
MAX_RECORD = 262144
OVERRIDES = {
    "CARGO",
    "CARGO_BUILD_RUSTC",
    "CARGO_BUILD_RUSTDOC",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    "RUSTC",
    "RUSTDOC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_ENCODED_RUSTDOCFLAGS",
    "RUSTC_BOOTSTRAP",
    "RUSTUP_TOOLCHAIN",
    "CARGO_BUILD_TARGET",
    "CARGO_BUILD_RUSTFLAGS",
    "CARGO_BUILD_RUSTDOCFLAGS",
    "CLIPPY_DRIVER_PATH",
    "RUSTFMT",
}
PROBE_FILES = {
    "Cargo.toml": '[package]\nname="toolchain_identity_probe"\nversion="0.0.0"\nedition="2021"\n[workspace]\n',
    "Cargo.lock": 'version = 3\n[[package]]\nname = "toolchain_identity_probe"\nversion = "0.0.0"\n',
    "src/lib.rs": "/// A tiny tool identity fixture.\npub fn identity_probe() -> u8 { 1 }\n",
    "build.rs": """use std::{env,fs,path::PathBuf,process::Command};
fn main() {
    let directory=PathBuf::from(env::var("IDENTITY_PROBE_DIR").unwrap());
    for (tool,key) in [("cargo","CARGO"),("rustc","RUSTC"),("rustdoc","RUSTDOC")] {
        let executable=env::var(key).expect("Cargo must expose its selected child tools");
        fs::write(directory.join(format!("{tool}.path")),&executable).unwrap();
        let output=Command::new(&executable).args(["--version","--verbose"]).output().unwrap();
        assert!(output.status.success());
        fs::write(directory.join(format!("{tool}.version")),output.stdout).unwrap();
        if tool=="rustc" {
            let output=Command::new(&executable).args(["--print","sysroot"]).output().unwrap();
            assert!(output.status.success());
            fs::write(directory.join("rustc.sysroot"),output.stdout).unwrap();
        }
    }
}
""",
}


def sha256(raw):
    return hashlib.sha256(raw).hexdigest()


def safe_path(path):
    path = Path(path).absolute()
    if any(character in str(path) for character in ("\n", "\r", "\0")):
        raise ValueError("unsafe environment-file path")
    return path


def pin_kind(pin):
    if re.fullmatch(r"[0-9]+\.[0-9]+(?:\.[0-9]+)?", pin):
        return {
            "kind": "release",
            "release": pin if pin.count(".") == 2 else pin + ".0",
        }
    if re.fullmatch(r"nightly-[0-9]{4}-[0-9]{2}-[0-9]{2}", pin):
        date.fromisoformat(pin[8:])
        return {"kind": "dated-nightly", "date": pin[8:]}
    raise ValueError("toolchain must be an exact release/MSRV or dated nightly")


def selection_override(key):
    return key in OVERRIDES or (
        key.startswith("CARGO_TARGET_") and key.endswith("_RUNNER")
    )


def reject_overrides(environment):
    rejected = sorted(
        key
        for key in environment
        if selection_override(key)
        and (key == "RUSTUP_TOOLCHAIN" or environment.get(key))
    )
    if rejected:
        raise ValueError(
            "inherited tool override is unsupported: " + ", ".join(rejected)
        )


def actual_rustup_home(environment):
    path = Path(
        environment.get(
            "RUSTUP_HOME",
            str(Path(environment.get("HOME", str(Path.home()))) / ".rustup"),
        )
    ).resolve(strict=True)
    if not path.is_dir() or not (path / "toolchains").is_dir():
        raise ValueError("installed rustup home is unavailable")
    return path


def audit_cargo_configs(environment, cwd):
    """Reject selection overrides in real caller config, without reading credentials."""
    cwd = Path(cwd).resolve(strict=True)
    home = Path(
        environment.get(
            "CARGO_HOME",
            str(Path(environment.get("HOME", str(Path.home()))) / ".cargo"),
        )
    ).absolute()
    directories = [home, *(parent / ".cargo" for parent in [cwd, *cwd.parents])]
    records = []
    seen = set()
    for directory in directories:
        if directory.is_symlink():
            raise ValueError(
                "Cargo configuration directory is a symlink: " + str(directory)
            )
        directory = directory.resolve()
        if directory in seen:
            continue
        seen.add(directory)
        for name in ("config", "config.toml"):
            path = directory / name
            if path.is_symlink():
                raise ValueError("Cargo configuration is a symlink: " + str(path))
            if not path.exists():
                continue
            if not path.is_file() or path.stat().st_size > MAX_OUTPUT:
                raise ValueError(
                    "Cargo configuration exceeds inspection bounds: " + str(path)
                )
            raw = path.read_bytes()
            if len(raw) > MAX_OUTPUT:
                raise ValueError(
                    "Cargo configuration exceeds inspection bounds: " + str(path)
                )
            try:
                data = tomllib.loads(raw.decode("utf-8"))
            except (ValueError, UnicodeError):
                raise ValueError(
                    "Cargo configuration parse failure: " + str(path)
                ) from None
            keys = []

            def visit(value, prefix=()):
                if not isinstance(value, dict):
                    return
                for key, child in value.items():
                    item = (*prefix, key)
                    keys.append(".".join(item))
                    if key == "include":
                        raise ValueError(
                            "Cargo configuration includes are unsupported: " + str(path)
                        )
                    visit(child, item)

            visit(data)
            build, target, forced = (
                data.get("build", {}),
                data.get("target", {}),
                data.get("env", {}),
            )
            if not all(
                isinstance(section, dict) for section in (build, target, forced)
            ):
                raise ValueError(
                    "Cargo selection configuration has an unsupported shape: "
                    + str(path)
                )
            forbidden = [
                "build." + key
                for key in build
                if key
                in {
                    "rustc",
                    "rustdoc",
                    "rustc-wrapper",
                    "rustc-workspace-wrapper",
                    "target",
                    "rustflags",
                    "rustdocflags",
                }
            ]
            for selector, settings in target.items():
                if not isinstance(settings, dict):
                    raise ValueError(
                        "Cargo target configuration has an unsupported shape: "
                        + str(path)
                    )
                if "runner" in settings:
                    forbidden.append("target." + selector + ".runner")
            for key in forced:
                if (
                    selection_override(key)
                    or key.startswith(("LD_", "DYLD_"))
                    or key
                    in {
                        "PATH",
                        "CARGO_HOME",
                        "RUSTUP_HOME",
                        "RUSTUP_TOOLCHAIN",
                    }
                ):
                    forbidden.append("env." + key)
            if forbidden:
                raise ValueError(
                    "Cargo selection override in "
                    + str(path)
                    + ": "
                    + ", ".join(sorted(forbidden))
                )
            records.append(
                {"path": str(path), "sha256": sha256(raw), "keys": sorted(keys)}
            )
    return records


def limited_environment(environment, proxy_dir, pin, directory):
    # Every subprocess receives only these explicit values. Loader, proxy and
    # provider-secret variables from the caller are never propagated.
    directory = Path(directory)
    home, temporary = directory / "home", directory / "tmp"
    (home / ".cargo").mkdir(parents=True, exist_ok=True)
    temporary.mkdir(parents=True, exist_ok=True)
    return {
        "PATH": os.pathsep.join(
            [str(proxy_dir), "/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        ),
        "HOME": str(home),
        "CARGO_HOME": str(home / ".cargo"),
        "RUSTUP_HOME": str(actual_rustup_home(environment)),
        "RUSTUP_TOOLCHAIN": pin,
        "RUSTUP_AUTO_INSTALL": "0",
        "CARGO_NET_OFFLINE": "true",
        "LANG": "C",
        "LC_ALL": "C",
        "TMPDIR": str(temporary),
        "TMP": str(temporary),
        "TEMP": str(temporary),
    }


def repository_default(cwd, environment, run):
    """Bind the nearest real default file to this checkout's immutable HEAD."""
    cwd = Path(cwd).resolve(strict=True)
    git = shutil.which("git", path="/usr/bin:/bin")
    if git is None:
        raise ValueError("caller default selection requires system Git")
    selected = {
        **environment,
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": os.devnull,
        "GIT_NO_REPLACE_OBJECTS": "1",
        "GIT_TERMINAL_PROMPT": "0",
        "GIT_OPTIONAL_LOCKS": "0",
    }

    def query(*arguments):
        try:
            return run(
                [
                    git,
                    "--no-optional-locks",
                    "-c",
                    "core.fsmonitor=false",
                    "-c",
                    "core.hooksPath=" + os.devnull,
                    *arguments,
                ],
                environment=selected,
                cwd=cwd,
            )["stdout"]
        except (ValueError, OSError, subprocess.TimeoutExpired):
            raise ValueError("caller default Git binding failed") from None

    found_root = query("rev-parse", "--show-toplevel").strip()
    if not Path(found_root).is_absolute():
        raise ValueError("caller checkout root must be absolute")
    root = safe_path(found_root).resolve(strict=True)
    if not cwd.is_relative_to(root):
        raise ValueError("caller cwd is outside its checkout")
    revision = query("rev-parse", "--verify", "HEAD^{commit}").strip()
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("caller checkout revision is malformed")
    path = None
    for directory in (cwd, *cwd.parents):
        legacy = directory / "rust-toolchain"
        if legacy.exists() or legacy.is_symlink():
            raise ValueError("plain rust-toolchain files are unsupported; use rust-toolchain.toml")
        candidate = directory / "rust-toolchain.toml"
        if candidate.exists() or candidate.is_symlink():
            path = candidate
            break
        if directory == root:
            break
    if path is None:
        raise ValueError("caller needs a checked-in rust-toolchain.toml default")
    info = path.lstat()
    if (
        path.is_symlink()
        or not stat.S_ISREG(info.st_mode)
        or info.st_nlink != 1
        or not 0 < info.st_size <= MAX_OUTPUT
    ):
        raise ValueError("caller default TOML is aliased or exceeds bounds")
    raw = path.read_bytes()
    if len(raw) != info.st_size or len(raw) > MAX_OUTPUT:
        raise ValueError("caller default TOML changed while reading")
    try:
        data = tomllib.loads(raw.decode("utf-8"))
    except (ValueError, UnicodeError):
        raise ValueError("caller default TOML is malformed") from None
    if (
        set(data) != {"toolchain"}
        or not isinstance(data["toolchain"], dict)
        or "path" in data["toolchain"]
    ):
        raise ValueError("caller default TOML has unsupported selection settings")
    channel = data["toolchain"].get("channel")
    if not isinstance(channel, str):
        raise ValueError("caller default TOML lacks an immutable channel")
    pin_kind(channel)
    relative = path.relative_to(root).as_posix()
    committed = query("show", revision + ":" + relative).encode("utf-8")
    if (
        raw != committed
        or query("rev-parse", "--verify", "HEAD^{commit}").strip() != revision
    ):
        raise ValueError(
            "caller default TOML differs from the checked-in bytes"
        )
    return {
        "path": str(path),
        "repositoryRoot": str(root),
        "repositoryRevision": revision,
        "repositoryRelativePath": relative,
        "sha256": sha256(raw),
        "channel": channel,
    }


def caller_selection(cwd, environment, proxies, run):
    # This environment deliberately has no forced toolchain. Rustup must read
    # the real cwd and its actual installed override settings before isolation.
    selected = {
        key: value for key, value in environment.items() if key != "RUSTUP_TOOLCHAIN"
    }
    default = repository_default(cwd, selected, run)
    cwd = Path(cwd).resolve(strict=True)
    active = run(
        [proxies["rustup"], "show", "active-toolchain"], environment=selected, cwd=cwd
    )
    match = re.fullmatch(
        r"([A-Za-z0-9_.-]+) \(overridden by '([^\n\r]+)'\)\n?", active["stdout"]
    )
    if (
        not match
        or match[2] != default["path"]
        or not match[1].startswith(default["channel"] + "-")
    ):
        raise ValueError(
            "actual caller Rustup selection does not use the repository default"
        )
    root = actual_rustup_home(selected) / "toolchains" / match[1]
    if (
        not root.is_dir()
        or root.resolve().parent
        != (actual_rustup_home(selected) / "toolchains").resolve()
    ):
        raise ValueError(
            "caller default is outside the installed Rustup toolchain store"
        )
    selector, tools = pin_kind(default["channel"]), {}
    for tool in TOOLS:
        found = run([proxies["rustup"], "which", tool], environment=selected, cwd=cwd)[
            "stdout"
        ].strip()
        path = safe_path(found)
        if (
            not Path(found).is_absolute()
            or path.resolve() != (root / "bin" / tool).resolve()
            or not path.is_file()
            or not os.access(path, os.X_OK)
        ):
            raise ValueError(
                "plain caller tool resolution differs from its active default: " + tool
            )
        observed = run(
            [proxies["aliases"][tool]["path"], "--version", "--verbose"],
            environment=selected,
            cwd=cwd,
        )
        release = observed_version(tool, observed["stdout"], selector)
        absolute = run(
            [str(path), "--version", "--verbose"], environment=selected, cwd=cwd
        )
        if absolute["stdout"] != observed["stdout"]:
            raise ValueError("plain caller proxy and resolved default differ: " + tool)
        tools[tool] = {"path": str(path), "release": release, **observed}
    if len({value["release"] for value in tools.values()}) != 1:
        raise ValueError("plain caller Cargo/compiler/rustdoc defaults differ")
    repeated_active = run(
        [proxies["rustup"], "show", "active-toolchain"], environment=selected, cwd=cwd
    )
    if repeated_active != active or repository_default(cwd, selected, run) != default:
        raise ValueError("caller default changed during selection check")
    return {
        "cwd": str(cwd),
        "rustupHome": str(actual_rustup_home(selected)),
        "repositoryDefault": default,
        "activeToolchain": {"name": match[1], **active},
        "defaultTools": tools,
        "matchesRepositoryDefault": True,
        "forcedToolchain": False,
    }


def command(argv, *, environment, cwd=None):
    process = subprocess.run(
        argv, cwd=cwd, env=environment, text=True, capture_output=True, timeout=60
    )
    if (
        len(process.stdout.encode()) > MAX_OUTPUT
        or len(process.stderr.encode()) > MAX_OUTPUT
    ):
        raise ValueError("tool output exceeds bounds")
    if process.returncode:
        raise ValueError(
            "tool run failed: "
            + Path(argv[0]).name
            + ": "
            + process.stderr[-2048:]
        )
    return {"stdout": process.stdout, "stderr": process.stderr}


def verify_proxies(directory):
    directory = safe_path(directory)
    rustup = directory / ("rustup.exe" if os.name == "nt" else "rustup")
    if not rustup.is_file() or not os.access(rustup, os.X_OK):
        raise ValueError("missing official rustup executable")
    rustup_hash = sha256(rustup.read_bytes())
    aliases = {}
    for tool in TOOLS:
        path = directory / (tool + (".exe" if os.name == "nt" else ""))
        if not path.is_file() or not os.access(path, os.X_OK):
            raise ValueError("missing rustup proxy: " + tool)
        if os.path.samefile(path, rustup):
            relation = "same-file-rustup-alias"
        elif sha256(path.read_bytes()) == rustup_hash:
            relation = "byte-identical-rustup-copy"
        else:
            raise ValueError("foreign executable in rustup proxy directory: " + tool)
        aliases[tool] = {"path": str(path), "relation": relation, "sha256": rustup_hash}
    return {
        "directory": str(directory),
        "rustup": str(rustup),
        "rustupSha256": rustup_hash,
        "aliases": aliases,
    }


def discover_proxies(environment):
    selected = shutil.which("rustup", path=environment.get("PATH"))
    candidates = []
    if environment.get("CARGO_HOME"):
        candidates.append(Path(environment["CARGO_HOME"]) / "bin")
    if selected:
        candidates.append(Path(selected).parent)
        # Homebrew exposes rustup through a symlink, beside unrelated Rust
        # binaries. Its resolved installation can contain the actual proxies.
        # verify_proxies still requires every tool to alias rustup itself.
        installation_bin = Path(selected).resolve().parent
        candidates.append(installation_bin)
        # Homebrew env-script wrappers live in bin; the real manager and
        # its aliases live together in libexec/bin in the same keg.
        candidates.append(installation_bin.parent / "libexec/bin")
    candidates.append(Path(environment.get("HOME", str(Path.home()))) / ".cargo/bin")
    for directory in dict.fromkeys(candidates):
        try:
            return verify_proxies(directory)
        except ValueError:
            continue
    raise ValueError("no verified official rustup proxy directory is available")


def observed_version(tool, raw, selector):
    match = re.match(
        r"^" + tool + r" ([0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?) ", raw
    )
    if not match:
        raise ValueError("malformed tool version: " + tool)
    release = match[1]
    if selector["kind"] == "release" and release != selector["release"]:
        raise ValueError(
            "observed tool version differs from requested release: " + tool
        )
    if selector["kind"] == "dated-nightly" and not release.endswith("-nightly"):
        raise ValueError("safety toolchain did not report a nightly: " + tool)
    if "\nrelease: " + release not in raw:
        raise ValueError("verbose release identity is missing: " + tool)
    return release


def parent_identity(pin, environment, proxies, run=command, *, execution_environment):
    selector = pin_kind(pin)
    selected = execution_environment
    rustup_version = run([proxies["rustup"], "--version"], environment=selected)
    if not rustup_version["stdout"].startswith("rustup "):
        raise ValueError("selected proxy manager is not rustup")
    tools = {}
    proxy_versions = {}
    managed = (actual_rustup_home(environment) / "toolchains").resolve()
    for tool in TOOLS:
        found = run(
            [proxies["rustup"], "which", "--toolchain", pin, tool], environment=selected
        )["stdout"].strip()
        path = safe_path(found)
        if (
            not Path(found).is_absolute()
            or not path.is_file()
            or not os.access(path, os.X_OK)
        ):
            raise ValueError("rustup selected a missing/nonabsolute tool: " + tool)
        root = path.resolve().parent.parent
        if (
            root.parent != managed
            or not root.name.startswith(pin + "-")
            or path.resolve().parent.name != "bin"
        ):
            raise ValueError(
                "selected executable is outside the pinned rustup toolchain: " + tool
            )
        raw = run([str(path), "--version", "--verbose"], environment=selected)
        release = observed_version(tool, raw["stdout"], selector)
        tools[tool] = {
            "path": str(path),
            "sha256": sha256(path.read_bytes()),
            "release": release,
            **raw,
        }
        proxy = run(
            [proxies["aliases"][tool]["path"], "+" + pin, "--version", "--verbose"],
            environment=selected,
        )
        if proxy["stdout"] != raw["stdout"]:
            raise ValueError("official proxy selected a different tool: " + tool)
        proxy_versions[tool] = proxy
    roots = {Path(tool["path"]).resolve().parent.parent for tool in tools.values()}
    if len(roots) != 1:
        raise ValueError("selected tools do not share one rustup toolchain")
    root = roots.pop()
    if not str(root.name).startswith(pin + "-"):
        raise ValueError("toolchain directory does not match the requested pin")
    if root.parent != managed:
        raise ValueError("tools are not inside the installed rustup toolchain store")
    manifest_path = root / "lib/rustlib/multirust-channel-manifest.toml"
    manifest_raw = manifest_path.read_bytes()
    manifest = tomllib.loads(manifest_raw.decode())
    if selector["kind"] == "dated-nightly" and manifest.get("date") != selector["date"]:
        raise ValueError("installed nightly manifest date differs from the pin")
    for tool in ("rustc", "rustdoc"):
        if (
            tools[tool]["stdout"].splitlines()[0].split(" ", 1)[1]
            != manifest["pkg"]["rustc"]["version"]
        ):
            raise ValueError(
                "selected compiler differs from installed channel manifest"
            )
    cargo_manifest_suffix = manifest["pkg"]["cargo"]["version"].split(" ", 1)[1]
    cargo_observed_suffix = tools["cargo"]["stdout"].splitlines()[0].split(" ", 2)[2]
    if cargo_manifest_suffix != cargo_observed_suffix:
        raise ValueError("observed Cargo differs from installed channel manifest")
    if len({tool["release"] for tool in tools.values()}) != 1:
        raise ValueError("Cargo/compiler/rustdoc releases differ")
    sysroot = run([tools["rustc"]["path"], "--print", "sysroot"], environment=selected)[
        "stdout"
    ].strip()
    if Path(sysroot).resolve() != root:
        raise ValueError("compiler sysroot differs from installed toolchain")
    return {
        "pin": pin,
        "selector": selector,
        "toolchainRoot": str(root),
        "tools": tools,
        "rustupVersion": rustup_version,
        "proxyVersions": proxy_versions,
        "channelManifest": {
            "path": str(manifest_path),
            "sha256": sha256(manifest_raw),
            "date": manifest.get("date"),
        },
    }, selected


def cargo_commands(log):
    result = []
    for line in log.splitlines():
        match = re.search(r"Running `(.*)`$", line)
        if not match:
            continue
        parts = shlex.split(match[1])
        while parts and re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", parts[0]):
            parts.pop(0)
        if parts and Path(parts[0]).name in {
            "rustc",
            "rustdoc",
            "rustc.exe",
            "rustdoc.exe",
        }:
            result.append(
                {"tool": Path(parts[0]).stem, "executable": parts[0], "argv": parts[1:]}
            )
    return result


def validate_child(parent, child):
    for tool in TOOLS:
        actual = child["tools"].get(tool, {})
        expected = parent["tools"][tool]
        if (
            not Path(actual.get("path", "")).is_absolute()
            or Path(actual["path"]).resolve() != Path(expected["path"]).resolve()
        ):
            raise ValueError("Cargo child selected a different executable: " + tool)
        if actual.get("stdout") != expected["stdout"]:
            raise ValueError("Cargo child reported a different version: " + tool)
    if Path(child["rustcSysroot"]).resolve() != Path(parent["toolchainRoot"]).resolve():
        raise ValueError("Cargo child compiler sysroot differs")
    for tool in ("rustc", "rustdoc"):
        matches = [item for item in child["invocations"] if item["tool"] == tool]
        if not matches or any(
            Path(item["executable"]).resolve()
            != Path(parent["tools"][tool]["path"]).resolve()
            for item in matches
        ):
            raise ValueError("actual Cargo invocation lacks the pinned child: " + tool)


def child_probe(parent, environment, proxies, run=command):
    with tempfile.TemporaryDirectory(prefix="pinewasm-tool-identity-") as temporary:
        directory = Path(temporary)
        crate, observed = directory / "crate", directory / "observed"
        observed.mkdir()
        for name, source in PROBE_FILES.items():
            path = crate / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(source)
        selected = {
            **environment,
            "CARGO_HOME": str(directory / "cargo-home"),
            "CARGO_TARGET_DIR": str(directory / "target"),
            "CARGO_NET_OFFLINE": "true",
            "IDENTITY_PROBE_DIR": str(observed),
        }
        argv = [
            proxies["aliases"]["cargo"]["path"],
            "+" + parent["pin"],
            "doc",
            "--offline",
            "--locked",
            "--no-deps",
            "--verbose",
            "--verbose",
            "--manifest-path",
            str(crate / "Cargo.toml"),
        ]
        working_root = directory / "working"
        working_root.mkdir()
        # The real caller was audited before isolation. Also reject unexpected
        # selection settings in any ancestor of this private check directory.
        audit_cargo_configs(selected, working_root)
        trace = run(argv, environment=selected, cwd=working_root)
        tools = {
            tool: {
                "path": (observed / (tool + ".path")).read_text(),
                "stdout": (observed / (tool + ".version")).read_text(),
            }
            for tool in TOOLS
        }
        result = {
            "tools": tools,
            "rustcSysroot": (observed / "rustc.sysroot").read_text().strip(),
            "invocations": cargo_commands(trace["stderr"]),
            "cargoCommand": argv,
            "stdout": trace["stdout"],
            "stderr": trace["stderr"],
            "fixtureSha256": {
                name: sha256(source.encode()) for name, source in PROBE_FILES.items()
            },
        }
        validate_child(parent, result)
        return result


def observe(pin, environment=None, run=command, *, cwd=None):
    environment = dict(os.environ if environment is None else environment)
    pin_kind(pin)
    reject_overrides(environment)
    caller_cwd = (Path.cwd() if cwd is None else Path(cwd)).resolve(strict=True)
    configurations = audit_cargo_configs(environment, caller_cwd)
    proxies = discover_proxies(environment)
    with tempfile.TemporaryDirectory(prefix="pinewasm-tool-selection-") as temporary:
        selected = limited_environment(
            environment, proxies["directory"], pin, temporary
        )
        caller = caller_selection(caller_cwd, selected, proxies, run)
        working = Path(temporary) / "working"
        working.mkdir()

        # Version checks also avoid the caller's home/cwd and ambient secrets.
        def isolated_run(argv, *, environment, cwd=None):
            return run(
                argv, environment=environment, cwd=working if cwd is None else cwd
            )

        parent, selected = parent_identity(
            pin, environment, proxies, isolated_run, execution_environment=selected
        )
        child = child_probe(parent, selected, proxies, isolated_run)
    return {
        "schemaVersion": 2,
        "kind": "parent-and-cargo-child-toolchain-identity",
        "requestedToolchain": pin,
        "proxies": proxies,
        "parent": parent,
        "child": child,
        "callerCargoConfiguration": configurations,
        "callerToolchainSelection": caller,
        "identityVerified": True,
        "laterWorkflowTestOutcome": "not asserted",
        "selectionScope": "Real caller compiler/target/runner configuration, nearest checked-in Rustup default and actual plain proxy selection are audited before isolated requested-toolchain/Cargo checks. Explicit MSRV/nightly checks may differ from the repository default. Checks cover installed tools and the tiny offline fixture only, not later tests or a default toolchain change.",
    }


def write_record(record, path, github_path, github_output=None):
    path, github_path = safe_path(path), safe_path(github_path)
    github_output = safe_path(github_output) if github_output is not None else None
    paths = [
        target.resolve()
        for target in (path, github_path, github_output)
        if target is not None
    ]
    if len(paths) != len(set(paths)):
        raise ValueError("record and environment files must be distinct")
    for target in (path, github_path, github_output):
        if target is not None and target.is_symlink():
            raise ValueError("environment/record files cannot be symlinks")
    if path.exists():
        raise ValueError("identity record must use a new evidence path")
    raw = (
        json.dumps(record, indent=2, sort_keys=True, allow_nan=False) + "\n"
    ).encode()
    if len(raw) > MAX_RECORD:
        raise ValueError("identity record exceeds bounds")
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("xb") as stream:
        stream.write(raw)
    proxy = safe_path(record["proxies"]["directory"])
    with github_path.open("a") as stream:
        stream.write(str(proxy) + "\n")
    outputs = {
        "proxy_dir": str(proxy),
        "record_path": str(path),
        "record_sha256": sha256(raw),
        **{
            tool + "_path": str(safe_path(record["parent"]["tools"][tool]["path"]))
            for tool in TOOLS
        },
    }
    if github_output is not None:
        with github_output.open("a") as stream:
            for key, value in outputs.items():
                stream.write(key + "=" + value + "\n")
    return outputs


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--toolchain", required=True)
    parser.add_argument("--record", required=True, type=Path)
    parser.add_argument("--github-path", required=True, type=Path)
    parser.add_argument("--github-output", type=Path)
    args = parser.parse_args()
    result = observe(args.toolchain)
    outputs = write_record(result, args.record, args.github_path, args.github_output)
    print(json.dumps({"identityVerified": True, **outputs}, sort_keys=True))


if __name__ == "__main__":
    main()
