use crate::command::{
    command_exists, git, git_bytes, nul_fields, output_allow_failure, repo_root, run, temp_dir,
};
use crate::TaskResult;
use std::fs;
use std::path::Path;
use std::process::Command;

pub(crate) fn check(args: &[String]) -> TaskResult {
    check_with_program(args, Path::new("gitleaks"))
}

pub(crate) fn check_with_program(args: &[String], program: &Path) -> TaskResult {
    if !command_exists(program) {
        return Err(
            "gitleaks is required; run the pinned installer through `cargo xtask install-gitleaks`"
                .into(),
        );
    }
    let root = repo_root()?;
    match args {
        [] => scan_working_tree(program, &root),
        [flag] if flag == "--self-test" => self_test(program),
        [flag] if flag == "--staged" => scan_staged(program, &root),
        [flag, base, head] if flag == "--git-range" => scan_range(program, &root, base, head),
        _ => Err(
            "usage: check-secrets [--self-test | --staged | --git-range BASE_SHA HEAD_SHA]".into(),
        ),
    }
}

fn scan_working_tree(program: &Path, root: &Path) -> TaskResult {
    let temporary = temp_dir("secret-scan")?;
    let tree = temporary.path().join("tree");
    fs::create_dir_all(&tree).map_err(|error| format!("cannot create scan tree: {error}"))?;
    let listing = git_bytes(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let mut copied = 0usize;
    for field in nul_fields(&listing) {
        let relative = std::str::from_utf8(field)
            .map_err(|error| format!("Git path is not UTF-8: {error}"))?;
        let source = root.join(relative);
        if !source.exists() && !source.is_symlink() {
            continue;
        }
        if source.is_symlink() {
            return Err(format!(
                "symbolic link cannot be included in exact secret scan: {relative}"
            ));
        }
        if source.is_file() {
            crate::command::copy_file(&source, &tree.join(relative))?;
            copied += 1;
        }
    }
    if copied == 0 {
        return Err("no publishable files found".into());
    }
    run_gitleaks_dir(program, &tree)
}

fn scan_staged(program: &Path, root: &Path) -> TaskResult {
    let temporary = temp_dir("secret-staged")?;
    let tree = temporary.path().join("tree");
    fs::create_dir_all(&tree).map_err(|error| format!("cannot create scan tree: {error}"))?;
    export_index_blobs(root, &tree)?;
    run_gitleaks_dir(program, &tree)
}

fn scan_range(program: &Path, root: &Path, base: &str, head: &str) -> TaskResult {
    require_sha(base)?;
    require_sha(head)?;
    verify_commit(root, head)?;
    if !base.bytes().all(|byte| byte == b'0') {
        verify_commit(root, base)?;
    }
    let options = history_log_options(base, head);
    run(Command::new(program).args(["git"]).arg(root).args([
        "--log-opts",
        &options,
        "--no-banner",
        "--redact",
        "--timeout",
        "120",
        "--max-archive-depth",
        "2",
    ]))
}

fn run_gitleaks_dir(program: &Path, tree: &Path) -> TaskResult {
    run(Command::new(program).arg("dir").arg(tree).args([
        "--no-banner",
        "--redact",
        "--timeout",
        "120",
        "--max-archive-depth",
        "2",
    ]))
}

fn export_index_blobs(root: &Path, destination: &Path) -> TaskResult {
    let listing = git_bytes(root, &["ls-files", "--stage", "-z"])?;
    let mut exported = 0usize;
    for field in nul_fields(&listing) {
        let entry = std::str::from_utf8(field)
            .map_err(|error| format!("index entry is not UTF-8: {error}"))?;
        let (metadata, relative) = entry
            .split_once('\t')
            .ok_or_else(|| format!("malformed index entry: {entry:?}"))?;
        let mut parts = metadata.split_whitespace();
        let mode = parts.next().unwrap_or_default();
        let object = parts.next().unwrap_or_default();
        let stage = parts.next().unwrap_or_default();
        if parts.next().is_some() || object.len() != 40 {
            return Err(format!("malformed index metadata for {relative}"));
        }
        if stage != "0" {
            return Err(format!(
                "unmerged index entry cannot be scanned exactly: {relative}"
            ));
        }
        if mode == "160000" {
            return Err(format!(
                "gitlink cannot be materialized for an exact staged scan: {relative}"
            ));
        }
        if !matches!(mode, "100644" | "100755" | "120000") {
            return Err(format!(
                "unsupported index mode {mode} for staged scan: {relative}"
            ));
        }
        let bytes = git_bytes(root, &["cat-file", "blob", object])?;
        let output = destination.join(relative);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        fs::write(&output, bytes)
            .map_err(|error| format!("cannot materialize {relative}: {error}"))?;
        exported += 1;
    }
    if exported == 0 {
        return Err("no staged repository files found".into());
    }
    Ok(())
}

fn self_test(program: &Path) -> TaskResult {
    index_export_self_test()?;
    merge_history_self_test(program)?;
    Ok(())
}

fn index_export_self_test() -> TaskResult {
    let temporary = temp_dir("secret-index-self-test")?;
    let root = temporary.path().join("repository");
    let exact = temporary.path().join("exact");
    let checkout = temporary.path().join("checkout");
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    fs::create_dir_all(&exact).map_err(|error| error.to_string())?;
    fs::create_dir_all(&checkout).map_err(|error| error.to_string())?;
    git_run(&root, &["init", "--quiet"])?;
    git_run(&root, &["config", "filter.redact.clean", "cat"])?;
    git_run(
        &root,
        &[
            "config",
            "filter.redact.smudge",
            "sed 's/SELF_TEST_SECRET_TOKEN/REDACTED/g'",
        ],
    )?;
    fs::write(root.join(".gitattributes"), "secret.txt filter=redact\n")
        .map_err(|error| error.to_string())?;
    fs::write(root.join("secret.txt"), "SELF_TEST_SECRET_TOKEN\n")
        .map_err(|error| error.to_string())?;
    git_run(&root, &["add", "--", ".gitattributes", "secret.txt"])?;
    run(Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["checkout-index", "--all"])
        .arg(format!("--prefix={}/", checkout.display())))?;
    if fs::read_to_string(checkout.join("secret.txt"))
        .map_err(|error| error.to_string())?
        .trim()
        != "REDACTED"
    {
        return Err("staged-scan self-test did not activate its hostile smudge filter".into());
    }
    export_index_blobs(&root, &exact)?;
    if fs::read_to_string(exact.join("secret.txt"))
        .map_err(|error| error.to_string())?
        .trim()
        != "SELF_TEST_SECRET_TOKEN"
    {
        return Err("exact index export did not preserve staged blob bytes".into());
    }
    println!("Exact staged-blob export self-test: clean.");
    Ok(())
}

fn merge_history_self_test(program: &Path) -> TaskResult {
    let temporary = temp_dir("secret-history-self-test")?;
    let root = temporary.path();
    git_run(root, &["init", "--quiet"])?;
    fs::create_dir_all(root.join(".git/no-hooks")).map_err(|error| error.to_string())?;
    git_run(root, &["config", "core.hooksPath", ".git/no-hooks"])?;
    git_run(root, &["config", "user.name", "Secret History Test"])?;
    git_run(
        root,
        &[
            "config",
            "user.email",
            "secret-history-test@example.invalid",
        ],
    )?;
    git_run(root, &["config", "commit.gpgsign", "false"])?;
    git_run(root, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
    fs::write(root.join("root.txt"), "root\n").map_err(|error| error.to_string())?;
    git_run(root, &["add", "root.txt"])?;
    git_run(root, &["commit", "--quiet", "-m", "root"])?;
    let root_commit = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    git_run(root, &["checkout", "--quiet", "-b", "topic"])?;
    fs::write(root.join("topic.txt"), "topic\n").map_err(|error| error.to_string())?;
    git_run(root, &["add", "topic.txt"])?;
    git_run(root, &["commit", "--quiet", "-m", "topic"])?;
    git_run(root, &["checkout", "--quiet", "main"])?;
    fs::write(root.join("main.txt"), "main\n").map_err(|error| error.to_string())?;
    git_run(root, &["add", "main.txt"])?;
    git_run(root, &["commit", "--quiet", "-m", "main"])?;
    git_run(
        root,
        &["merge", "--quiet", "--no-ff", "--no-commit", "topic"],
    )?;
    // Synthetic, non-secret scanner self-test value, assembled at runtime.
    let sentinel = format!("-----BEGIN OPENSSH {}-----\nb3BlbnNzaC1rZXktdjEAAAAABHNlbGYtdGVzdAAAAA==\n-----END OPENSSH {}-----\n", "PRIVATE KEY", "PRIVATE KEY");
    fs::write(root.join("merge-only-credential.txt"), sentinel)
        .map_err(|error| error.to_string())?;
    git_run(root, &["add", "merge-only-credential.txt"])?;
    git_run(
        root,
        &["commit", "--quiet", "-m", "merge with independent fixture"],
    )?;
    git_run(root, &["rm", "--quiet", "merge-only-credential.txt"])?;
    git_run(root, &["commit", "--quiet", "-m", "delete fixture"])?;
    let head = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    for options in [
        history_log_options(&root_commit, &head),
        history_log_options("0000000000000000000000000000000000000000", &head),
    ] {
        let result = output_allow_failure(Command::new(program).arg("git").arg(root).args([
            "--log-opts",
            &options,
            "--no-banner",
            "--redact",
            "--timeout",
            "30",
            "--exit-code",
            "17",
        ]))?;
        if result.status.code() != Some(17) {
            return Err(format!(
                "merge-history self-test expected a finding for {options}, got {}\n{}",
                result.status,
                String::from_utf8_lossy(&result.stdout)
            ));
        }
    }
    println!("Merge-only secret-history self-test: ordinary and initial-push ranges rejected.");
    Ok(())
}

fn history_log_options(base: &str, head: &str) -> String {
    if base.bytes().all(|byte| byte == b'0') {
        format!("{head} --diff-merges=separate")
    } else {
        format!("{base}..{head} --diff-merges=separate")
    }
}

fn require_sha(value: &str) -> TaskResult {
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("BASE_SHA and HEAD_SHA must be exact 40-character Git object ids".into())
    }
}

fn verify_commit(root: &Path, sha: &str) -> TaskResult {
    let result = output_allow_failure(Command::new("git").arg("-C").arg(root).args([
        "cat-file",
        "-e",
        &format!("{sha}^{{commit}}"),
    ]))?;
    if result.status.success() {
        Ok(())
    } else {
        Err(format!("not a commit in this repository: {sha}"))
    }
}

fn git_run(root: &Path, args: &[&str]) -> TaskResult {
    run(Command::new("git").arg("-C").arg(root).args(args))
}
