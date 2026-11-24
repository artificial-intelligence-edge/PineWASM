use crate::command::{copy_file, output_allow_failure, run, sha256_file, temp_dir};
use crate::TaskResult;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn install_actionlint(args: &[String]) -> TaskResult {
    let (version, prefix) = two_args("install-actionlint", args)?;
    let architecture = normalized_arch()?;
    let operating_system = normalized_os()?;
    let (release_os, release_arch, archive_hash) =
        match (version, operating_system.as_str(), architecture.as_str()) {
        ("1.7.12", "linux", "arm64") => (
            "linux", "arm64", "325e971b6ba9bfa504672e29be93c24981eeb1c07576d730e9f7c8805afff0c6",
        ),
        ("1.7.12", "linux", "x86_64") => (
            "linux", "amd64", "8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8",
        ),
        ("1.7.12", "macos", "arm64") => (
            "darwin", "arm64", "aba9ced2dee8d27fecca3dc7feb1a7f9a52caefa1eb46f3271ea66b6e0e6953f",
        ),
        ("1.7.12", "macos", "x86_64") => (
            "darwin", "amd64", "5b44c3bc2255115c9b69e30efc0fecdf498fdb63c5d58e17084fd5f16324c644",
        ),
        _ => {
            return Err(format!(
                "unsupported pinned actionlint version/platform: {version}/{operating_system}/{architecture}"
            ))
        }
    };
    let archive = format!("actionlint_{version}_{release_os}_{release_arch}.tar.gz");
    let url = format!("https://github.com/rhysd/actionlint/releases/download/v{version}/{archive}");
    let temporary = download_archive(&url, &archive, archive_hash)?;
    extract(temporary.path().join(&archive).as_path(), temporary.path())?;
    let destination = prefix.join("bin/actionlint");
    install_file(&temporary.path().join("actionlint"), &destination, true)?;
    let output = output_allow_failure(Command::new(&destination).arg("-version"))?;
    if !output.status.success()
        || !String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.trim() == version)
    {
        return Err("installed actionlint did not report the pinned version".into());
    }
    Ok(())
}

pub(crate) fn install_binaryen(args: &[String]) -> TaskResult {
    let (version, prefix) = two_args("install-binaryen", args)?;
    let architecture = normalized_arch()?;
    let operating_system = normalized_os()?;
    let (release_arch, release_os, archive_hash, binary_hash, library) =
        match (version, operating_system.as_str(), architecture.as_str()) {
            ("131", "linux", "arm64") => ("aarch64", "linux", "ba991f677edd9a21d2bc96c0144bc8ac5b112d4d98a3eb266e075e22e557df2a", "573d9420ea76dbc1e17b5b7debc1de4182f541e59fa4238761b658d795ddb167", None),
            ("131", "linux", "x86_64") => ("x86_64", "linux", "b5bf1f0eaf17c63ee588ff7a5954dc8f6ce2c26989051c66f24dfe9ece3e46db", "bad4b6524b2c8e4b27b9aa69bde1a4b9a05ec8887c77ef0d34300f5825acd97c", None),
            ("131", "macos", "arm64") => ("arm64", "macos", "e441b48dc22163d209b4f05e44dc7210909b01237642b6c9ae48fd710a3ef83e", "13560cec4415f7fbc0225d8f31168b0d45674e6763f1d93225936cf7d000288f", Some(("libbinaryen.dylib", "41916b14f654eff44814d21ed0d73a21913819378fc70dc524cd21430e9db476"))),
            ("131", "macos", "x86_64") => ("x86_64", "macos", "d209fadd8a894bdaf3bd3612a23c32a0af184d2f4a979b8c789e6e4f6a4de883", "e022347d1375780a09fa412ccef91dfeb136a6a29d6ed5a3101ab42fdba086ea", Some(("libbinaryen.dylib", "ef0aea581645fe624dd9049da13889784e009cc7734717236b4630ddd06a2688"))),
            _ => return Err(format!("unsupported pinned Binaryen version/platform: {version}/{operating_system}/{architecture}")),
        };
    let installed_binary = prefix.join("bin/wasm-opt");
    if binaryen_verified(&installed_binary, binary_hash, &prefix, library, version)? {
        return Ok(());
    }
    let release = format!("binaryen-version_{version}-{release_arch}-{release_os}");
    let archive = format!("{release}.tar.gz");
    let url = format!(
        "https://github.com/WebAssembly/binaryen/releases/download/version_{version}/{archive}"
    );
    let temporary = download_archive(&url, &archive, archive_hash)?;
    extract(&temporary.path().join(&archive), temporary.path())?;
    let extracted = temporary.path().join(format!("binaryen-version_{version}"));
    install_file(&extracted.join("bin/wasm-opt"), &installed_binary, true)?;
    if let Some((name, _)) = library {
        install_file(
            &extracted.join("lib").join(name),
            &prefix.join("lib").join(name),
            false,
        )?;
    }
    if !binaryen_verified(&installed_binary, binary_hash, &prefix, library, version)? {
        return Err("installed Binaryen files do not match the pinned digests".into());
    }
    Ok(())
}

pub(crate) fn install_gitleaks(args: &[String]) -> TaskResult {
    let (version, prefix) = two_args("install-gitleaks", args)?;
    let architecture = normalized_arch()?;
    let operating_system = normalized_os()?;
    let (platform, binary_hash, archive_hash) = match (version, operating_system.as_str(), architecture.as_str()) {
        ("8.30.1", "linux", "arm64") => ("linux_arm64", "00e91bbe655bd7c47753e8cfe61cb76ea1a5d7e7702fe161ee40102b46b3823b", "e4a487ee7ccd7d3a7f7ec08657610aa3606637dab924210b3aee62570fb4b080"),
        ("8.30.1", "linux", "x86_64") => ("linux_x64", "88f91962aa2f93ac6ab281d553b9e125f5197bbbce38f9f2437f7299c32e5509", "551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb"),
        ("8.30.1", "macos", "arm64") => ("darwin_arm64", "ba52fb1bfabbcde42f032afad3d6e0b19dff8ed105229a16e7caa338bbc0e84f", "b40ab0ae55c505963e365f271a8d3846efbc170aa17f2607f13df610a9aeb6a5"),
        ("8.30.1", "macos", "x86_64") => ("darwin_x64", "cee01fea7173f1b779dff188e1c26ecbcb4027d394acc573b23aaf0be260e291", "dfe101a4db2255fc85120ac7f3d25e4342c3c20cf749f2c20a18081af1952709"),
        _ => return Err(format!("unsupported pinned Gitleaks version/platform: {version}/{operating_system}/{architecture}")),
    };
    let destination = prefix.join("bin/gitleaks");
    if destination.is_file() && sha256_file(&destination)? == binary_hash {
        return Ok(());
    }
    if destination.exists() {
        eprintln!("Cached Gitleaks does not match its pinned digest; reinstalling.");
    }
    let archive = format!("gitleaks_{version}_{platform}.tar.gz");
    let url =
        format!("https://github.com/gitleaks/gitleaks/releases/download/v{version}/{archive}");
    let temporary = download_archive(&url, &archive, archive_hash)?;
    extract(&temporary.path().join(&archive), temporary.path())?;
    install_file(&temporary.path().join("gitleaks"), &destination, true)?;
    if sha256_file(&destination)? != binary_hash {
        let _ = fs::remove_file(&destination);
        return Err("installed Gitleaks does not match its pinned digest".into());
    }
    let output = output_allow_failure(Command::new(&destination).arg("version"))?;
    if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != version {
        return Err("installed Gitleaks did not report the pinned version".into());
    }
    Ok(())
}

fn two_args<'a>(command: &str, args: &'a [String]) -> TaskResult<(&'a str, PathBuf)> {
    if args.len() != 2 || args[1].is_empty() {
        return Err(format!("usage: {command} VERSION INSTALL_PREFIX"));
    }
    Ok((&args[0], PathBuf::from(&args[1])))
}

fn normalized_arch() -> TaskResult<String> {
    match env::consts::ARCH {
        "aarch64" => Ok("arm64".into()),
        "x86_64" => Ok("x86_64".into()),
        other => Err(format!("unsupported host architecture: {other}")),
    }
}

fn normalized_os() -> TaskResult<String> {
    match env::consts::OS {
        "linux" => Ok("linux".into()),
        "macos" => Ok("macos".into()),
        other => Err(format!("unsupported host operating system: {other}")),
    }
}

fn download_archive(
    url: &str,
    archive: &str,
    expected_hash: &str,
) -> TaskResult<crate::command::TempDir> {
    let temporary = temp_dir("download")?;
    let destination = temporary.path().join(archive);
    run(Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--proto",
            "=https",
            "--retry",
            "3",
            "--show-error",
            "--silent",
            "--output",
        ])
        .arg(&destination)
        .arg(url))?;
    let actual = sha256_file(&destination)?;
    if actual != expected_hash {
        return Err(format!(
            "archive checksum mismatch for {archive}: expected {expected_hash}, got {actual}"
        ));
    }
    Ok(temporary)
}

fn extract(archive: &Path, destination: &Path) -> TaskResult {
    run(Command::new("tar")
        .args(["--extract", "--gzip", "--no-same-owner", "--file"])
        .arg(archive)
        .arg("--directory")
        .arg(destination))
}

fn install_file(source: &Path, destination: &Path, executable: bool) -> TaskResult {
    copy_file(source, destination)?;
    #[cfg(not(unix))]
    let _ = executable;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            destination,
            fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
        )
        .map_err(|error| {
            format!(
                "cannot set permissions on {}: {error}",
                destination.display()
            )
        })?;
    }
    Ok(())
}

fn binaryen_verified(
    binary: &Path,
    binary_hash: &str,
    prefix: &Path,
    library: Option<(&str, &str)>,
    version: &str,
) -> TaskResult<bool> {
    if !binary.is_file() || sha256_file(binary)? != binary_hash {
        return Ok(false);
    }
    if let Some((name, hash)) = library {
        let library = prefix.join("lib").join(name);
        if !library.is_file() || sha256_file(&library)? != hash {
            return Ok(false);
        }
    }
    let output = output_allow_failure(Command::new(binary).arg("--version"))?;
    Ok(output.status.success()
        && String::from_utf8_lossy(&output.stdout).trim()
            == format!("wasm-opt version {version} (version_{version})"))
}
