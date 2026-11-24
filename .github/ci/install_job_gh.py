#!/usr/bin/env python3
"""Install the pinned official GitHub CLI in this job's temporary tree."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import platform
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
import zipfile

VERSION = "2.96.0"
RELEASE = "https://github.com/cli/cli/releases/tag/v" + VERSION
ROOT = Path(__file__).resolve().parents[2]
BLOCK = 1024 * 1024
MAX_BINARY_BYTES = 128 * BLOCK
ASSETS = {
    ("Linux", "x86_64"): {
        "name": "gh_2.96.0_linux_amd64.tar.gz",
        "bytes": 14652560,
        "sha256": "83d5c2ccad5498f58bf6368acb1ab32588cf43ab3a4b1c301bf36328b1c8bd60",
    },
    ("Linux", "aarch64"): {
        "name": "gh_2.96.0_linux_arm64.tar.gz",
        "bytes": 13321232,
        "sha256": "06f86ec7103d41993b76cd78072f43595c34aaa56506d971d9860e67140bf909",
    },
    ("Darwin", "arm64"): {
        "name": "gh_2.96.0_macOS_arm64.zip",
        "bytes": 13950131,
        "sha256": "f23a0c37d963aacc3bed703ccbd59b41c5ca22101fab7f00eb2b7cad23aba463",
    },
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def sha(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(BLOCK), b""):
            digest.update(block)
    return digest.hexdigest()


def no_symlinks(path):
    current = Path(path.anchor)
    for part in path.parts[1:]:
        current /= part
        require(not current.is_symlink(), "job paths cannot traverse symlinks")


def context(environment):
    values = [environment.get(name, "") for name in ("RUNNER_TEMP", "GITHUB_PATH")]
    require(
        all(value and not any(c in value for c in "\r\n\0") for value in values),
        "RUNNER_TEMP and GITHUB_PATH must be explicit job paths",
    )
    temporary, command_file = map(Path, values)
    require(
        temporary.is_absolute() and command_file.is_absolute(),
        "job paths must be absolute",
    )
    no_symlinks(temporary)
    no_symlinks(command_file)
    require(
        temporary == temporary.resolve() and command_file == command_file.resolve(),
        "job paths must be canonical and contain no traversal",
    )
    require(
        temporary.is_dir()
        and temporary.resolve() != Path.home().resolve()
        and not temporary.resolve().is_relative_to(ROOT),
        "RUNNER_TEMP must be an existing job directory outside the source tree",
    )
    require(
        command_file.is_relative_to(temporary) and command_file.parent.is_dir(),
        "GITHUB_PATH must remain inside RUNNER_TEMP",
    )
    entry = command_file.lstat()
    require(
        stat.S_ISREG(entry.st_mode) and entry.st_nlink == 1,
        "GITHUB_PATH must be an existing unaliased regular file",
    )
    return temporary, command_file, (entry.st_dev, entry.st_ino)


def download(asset, destination):
    url = (
        "https://github.com/cli/cli/releases/download/v" + VERSION + "/" + asset["name"]
    )
    # The download sends no credentials.
    request = urllib.request.Request(
        url, headers={"User-Agent": "pinned-job-gh-bootstrap"}
    )
    with urllib.request.urlopen(request, timeout=60) as source, destination.open(
        "xb"
    ) as target:
        total = 0
        while block := source.read(BLOCK):
            total += len(block)
            require(
                total <= asset["bytes"],
                "official GitHub CLI download exceeds pinned size",
            )
            target.write(block)


def copy_binary(source, size, destination):
    require(0 < size <= MAX_BINARY_BYTES, "GitHub CLI binary size is outside bounds")
    with destination.open("xb") as target:
        remaining = size
        while remaining:
            block = source.read(min(BLOCK, remaining))
            require(block, "GitHub CLI binary is truncated")
            target.write(block)
            remaining -= len(block)
        require(not source.read(1), "GitHub CLI binary exceeds declared member size")
    destination.chmod(0o500)


def extract_binary(archive, asset, destination):
    """Copy only the one exact ordinary executable; never extract other members."""
    if asset["name"].endswith(".tar.gz"):
        member_name = asset["name"][:-7] + "/bin/gh"
        with tarfile.open(archive, "r:gz") as package:
            members = [member for member in package if member.name == member_name]
            require(
                len(members) == 1 and members[0].isfile() and members[0].sparse is None,
                "official GitHub CLI executable member is absent, duplicated or not ordinary",
            )
            with package.extractfile(members[0]) as source:
                copy_binary(source, members[0].size, destination)
    elif asset["name"].endswith(".zip"):
        member_name = asset["name"][:-4] + "/bin/gh"
        with zipfile.ZipFile(archive) as package:
            members = [
                member
                for member in package.infolist()
                if member.filename == member_name
            ]
            require(
                len(members) == 1,
                "official GitHub CLI executable member is absent or duplicated",
            )
            member = members[0]
            kind = stat.S_IFMT(member.external_attr >> 16)
            require(
                not member.is_dir()
                and kind in (0, stat.S_IFREG)
                and not member.flag_bits & 1,
                "official GitHub CLI executable member is not ordinary",
            )
            with package.open(member) as source:
                copy_binary(source, member.file_size, destination)
    else:
        raise ValueError("unsupported pinned GitHub CLI archive format")


def install_package(asset, temporary, fetch):
    """Install and probe an authenticated package independently of ambient PATH."""
    directory = Path(tempfile.mkdtemp(prefix="gh-" + VERSION + "-", dir=temporary))
    archive = directory / asset["name"]
    fetch(asset, archive)
    require(
        not archive.is_symlink()
        and archive.is_file()
        and archive.stat().st_size == asset["bytes"]
        and sha(archive) == asset["sha256"],
        "official GitHub CLI archive differs from pinned size/hash",
    )
    binary_directory = directory / "bin"
    binary_directory.mkdir(mode=0o700)
    binary = binary_directory / "gh"
    extract_binary(archive, asset, binary)
    binary_sha = sha(binary)
    clean_environment = {
        "PATH": str(binary_directory),
        "GH_CONFIG_DIR": str(directory / "config"),
        "XDG_CACHE_HOME": str(directory / "cache"),
        "GH_NO_UPDATE_NOTIFIER": "1",
        "GH_PROMPT_DISABLED": "1",
        "LANG": "C",
        "LC_ALL": "C",
    }
    result = subprocess.run(
        [str(binary), "--version"],
        capture_output=True,
        text=True,
        timeout=15,
        check=False,
        env=clean_environment,
        cwd=directory,
    )
    lines = result.stdout.strip().splitlines()
    require(
        result.returncode == 0
        and lines
        and lines[0].split()[:3] == ["gh", "version", VERSION]
        and RELEASE in lines
        and sha(binary) == binary_sha,
        "pinned GitHub CLI absolute-path version probe failed",
    )
    return {
        "directory": str(directory),
        "bin": str(binary_directory),
        "binary": str(binary),
        "binarySha256": binary_sha,
        "version": result.stdout.strip(),
        "archive": asset,
        "ambientGhUsed": False,
    }


def bootstrap(environment=None, system=None, machine=None, fetch=download):
    environment = dict(os.environ if environment is None else environment)
    selected = (system or platform.system(), machine or platform.machine())
    require(selected in ASSETS, "unsupported GitHub CLI platform; no fallback")
    temporary, command_file, identity = context(environment)
    result = install_package(ASSETS[selected], temporary, fetch)
    no_symlinks(command_file)
    descriptor = os.open(command_file, os.O_WRONLY | os.O_APPEND | os.O_NOFOLLOW)
    with os.fdopen(descriptor, "a", encoding="utf-8") as stream:
        entry = os.fstat(stream.fileno())
        require(
            stat.S_ISREG(entry.st_mode)
            and entry.st_nlink == 1
            and (entry.st_dev, entry.st_ino) == identity,
            "GITHUB_PATH identity changed before publication",
        )
        stream.write(result["bin"] + "\n")
    result.update(
        {
            "platform": list(selected),
            "python": platform.python_version(),
            "pathPublished": True,
            "credentialsUsedForInstall": False,
        }
    )
    return result


def main():
    try:
        print(json.dumps(bootstrap(), sort_keys=True))
        return 0
    except (
        ValueError,
        OSError,
        tarfile.TarError,
        zipfile.BadZipFile,
        subprocess.SubprocessError,
    ) as error:
        print("error: " + str(error), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
