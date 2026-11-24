use crate::command::{git, output_allow_failure, repo_root, run, temp_dir};
use crate::TaskResult;
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

pub(crate) fn check(args: &[String]) -> TaskResult {
    if args == ["--self-test"] {
        return self_test();
    }
    if args.len() != 2 {
        return Err("usage: check-dco [--self-test | BASE_SHA HEAD_SHA]".into());
    }
    check_range(&repo_root()?, &args[0], &args[1])
}

fn check_range(root: &Path, base: &str, head: &str) -> TaskResult {
    check_range_with_context(root, base, head, &CiContext::from_env(), true)
}

fn check_range_with_context(
    root: &Path,
    base: &str,
    head: &str,
    context: &CiContext,
    report: bool,
) -> TaskResult {
    require_sha(base)?;
    require_sha(head)?;
    verify_commit(root, head)?;
    let revision = if base.bytes().all(|byte| byte == b'0') {
        let roots = git(root, &["rev-list", "--max-parents=0", head])?
            .lines()
            .count();
        let commits = git(root, &["rev-list", "--count", head])?
            .trim()
            .parse::<usize>()
            .map_err(|error| format!("cannot count commits: {error}"))?;
        if roots != 1 || commits != 1 {
            return Err(format!(
                "a branch-creating push must be the single audited root commit (roots={roots}, commits={commits})"
            ));
        }
        head.to_string()
    } else {
        verify_commit(root, base)?;
        format!("{base}..{head}")
    };
    let commits = git(root, &["rev-list", &revision])?
        .lines()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if commits.is_empty() {
        if report {
            println!("DCO: no commits introduced; nothing to check.");
        }
        return Ok(());
    }
    let mut failures = Vec::new();
    let mut checked = 0usize;
    let mut exempt_merges = 0usize;
    for sha in commits {
        let parent_text = git(root, &["show", "--no-patch", "--format=%P", &sha])?;
        let parents: Vec<_> = parent_text.split_whitespace().collect();
        if parents.len() > 1
            && (git(
                root,
                &[
                    "diff-tree",
                    "--cc",
                    "--no-commit-id",
                    "--name-only",
                    "-r",
                    &sha,
                ],
            )?
            .trim()
            .is_empty()
                || matches_automatic_merge(root, &sha, &parents)?)
        {
            exempt_merges += 1;
            continue;
        }
        checked += 1;
        let author_name = show(root, &sha, "%an")?;
        let author_email = show(root, &sha, "%ae")?;
        let committer_name = show(root, &sha, "%cn")?;
        let committer_email = show(root, &sha, "%ce")?;
        let subject = show(root, &sha, "%s")?;
        let signoffs = show(root, &sha, "%(trailers:key=Signed-off-by,valueonly)")?;
        let author = format!("{author_name} <{author_email}>");
        if signoffs.trim().is_empty() {
            failures.push(format!(
                "Missing Signed-off-by: {} {subject}\n  author: {author}",
                &sha[..12]
            ));
            continue;
        }
        if dependabot_allowed(
            &author_name,
            &author_email,
            &committer_name,
            &committer_email,
            &signoffs,
            context,
        ) {
            continue;
        }
        if !signoffs.lines().any(|line| line.trim() == author) {
            failures.push(format!(
                "Signed-off-by does not match the author: {} {subject}\n  author: {author}\n  trailers: {}",
                &sha[..12],
                signoffs.lines().map(str::trim).collect::<Vec<_>>().join(";")
            ));
        }
    }
    if !failures.is_empty() {
        if report {
            for failure in &failures {
                eprintln!("ERROR: {failure}");
            }
        }
        return Err("DCO check failed. Every commit needs a sign-off matching its author. Add one with `git commit --amend --signoff`.".into());
    }
    if report {
        println!(
            "DCO: {checked} introduced commit(s) signed off; {exempt_merges} pure merge commit(s) inherited checked parent content."
        );
    }
    Ok(())
}

/// A combined diff also lists files changed independently by both parents.
/// For those merges, reproduce Git's automatic merge and require its complete
/// tree to match. Conflicts and merge-only edits still require a sign-off.
fn matches_automatic_merge(root: &Path, sha: &str, parents: &[&str]) -> TaskResult<bool> {
    let [left, right] = parents else {
        return Ok(false);
    };
    let result = output_allow_failure(Command::new("git").arg("-C").arg(root).args([
        "merge-tree",
        "--write-tree",
        left,
        right,
    ]))?;
    if !result.status.success() {
        return Ok(false);
    }
    let expected = String::from_utf8(result.stdout).map_err(|error| error.to_string())?;
    let actual = git(root, &["rev-parse", &format!("{sha}^{{tree}}")])?;
    Ok(expected.trim() == actual.trim())
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
        "rev-parse",
        "--quiet",
        "--verify",
        &format!("{sha}^{{commit}}"),
    ]))?;
    if result.status.success() {
        Ok(())
    } else {
        Err(format!(
            "not a commit in this repository: {sha}; fetch full history"
        ))
    }
}

fn show(root: &Path, sha: &str, format: &str) -> TaskResult<String> {
    Ok(git(
        root,
        &["show", "--no-patch", &format!("--format={format}"), sha],
    )?
    .trim()
    .to_string())
}

fn dependabot_allowed(
    author_name: &str,
    author_email: &str,
    committer_name: &str,
    committer_email: &str,
    signoffs: &str,
    context: &CiContext,
) -> bool {
    if author_name != "dependabot[bot]"
        || author_email != "49699333+dependabot[bot]@users.noreply.github.com"
        || committer_name != "GitHub"
        || committer_email != "noreply@github.com"
        || !signoffs
            .lines()
            .any(|line| line.trim() == "dependabot[bot] <support@github.com>")
        || !context.github_actions
    {
        return false;
    }
    match context.event_name.as_deref() {
        Some("pull_request") => {
            context.actor.as_deref() == Some("dependabot[bot]")
                && context
                    .head_ref
                    .as_deref()
                    .is_some_and(|head| head.starts_with("dependabot/"))
        }
        // Commits already on main. Scheduled and dispatched runs on main
        // check the same commits a push to main once did.
        Some("push" | "schedule" | "workflow_dispatch") => {
            context.git_ref.as_deref() == Some("refs/heads/main")
                && context.head_ref.as_deref().is_none_or(str::is_empty)
        }
        _ => false,
    }
}

#[derive(Default)]
struct CiContext {
    github_actions: bool,
    event_name: Option<String>,
    actor: Option<String>,
    head_ref: Option<String>,
    git_ref: Option<String>,
}

impl CiContext {
    fn from_env() -> Self {
        Self {
            github_actions: env::var("GITHUB_ACTIONS").as_deref() == Ok("true"),
            event_name: env::var("GITHUB_EVENT_NAME").ok(),
            actor: env::var("GITHUB_ACTOR").ok(),
            head_ref: env::var("GITHUB_HEAD_REF").ok(),
            git_ref: env::var("GITHUB_REF").ok(),
        }
    }

    fn github(event_name: &str, actor: &str, head_ref: &str, git_ref: &str) -> Self {
        Self {
            github_actions: true,
            event_name: Some(event_name.to_string()),
            actor: Some(actor.to_string()),
            head_ref: Some(head_ref.to_string()),
            git_ref: Some(git_ref.to_string()),
        }
    }
}

fn self_test() -> TaskResult {
    let temporary = temp_dir("dco-self-test")?;
    let root = temporary.path();
    let local = CiContext::default();
    git_run(root, ["init", "--quiet"])?;
    git_run(root, ["config", "user.name", "DCO Test"])?;
    git_run(root, ["config", "user.email", "dco-test@example.invalid"])?;
    git_run(root, ["config", "commit.gpgsign", "false"])?;
    fs::create_dir_all(root.join(".git/no-hooks")).map_err(|error| error.to_string())?;
    git_run(root, ["config", "core.hooksPath", ".git/no-hooks"])?;
    git_run(root, ["checkout", "--quiet", "-b", "main"])?;
    write(root, "root.txt", "root\n")?;
    let shared = "first\n1\n2\n3\n4\n5\n6\nlast\n";
    write(root, "shared.txt", shared)?;
    git_run(root, ["add", "root.txt"])?;
    git_run(root, ["add", "shared.txt"])?;
    git_run(root, ["commit", "--quiet", "--signoff", "-m", "root"])?;

    git_run(root, ["checkout", "--quiet", "-b", "pure-topic"])?;
    write(root, "topic.txt", "topic\n")?;
    write(root, "shared.txt", &shared.replace("last", "topic last"))?;
    git_run(root, ["add", "topic.txt"])?;
    git_run(root, ["add", "shared.txt"])?;
    git_run(root, ["commit", "--quiet", "--signoff", "-m", "topic"])?;
    git_run(root, ["checkout", "--quiet", "main"])?;
    write(root, "main.txt", "main\n")?;
    write(root, "shared.txt", &shared.replace("first", "main first"))?;
    git_run(root, ["add", "main.txt"])?;
    git_run(root, ["add", "shared.txt"])?;
    git_run(root, ["commit", "--quiet", "--signoff", "-m", "main"])?;
    let base = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    git_run(
        root,
        ["merge", "--quiet", "--no-ff", "--no-edit", "pure-topic"],
    )?;
    let merge = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    check_range_with_context(root, &base, &merge, &local, true)
        .map_err(|_| "DCO self-test rejected an unsigned pure merge".to_string())?;

    git_run(root, ["checkout", "--quiet", "-b", "patch-topic"])?;
    write(root, "patch-topic.txt", "patch topic\n")?;
    git_run(root, ["add", "patch-topic.txt"])?;
    git_run(
        root,
        ["commit", "--quiet", "--signoff", "-m", "patch topic"],
    )?;
    git_run(root, ["checkout", "--quiet", "main"])?;
    write(root, "patch-main.txt", "patch main\n")?;
    git_run(root, ["add", "patch-main.txt"])?;
    git_run(root, ["commit", "--quiet", "--signoff", "-m", "patch main"])?;
    let patch_base = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    git_run(
        root,
        ["merge", "--quiet", "--no-ff", "--no-commit", "patch-topic"],
    )?;
    write(root, "merge-only.txt", "merge-only\n")?;
    git_run(root, ["add", "merge-only.txt"])?;
    git_run(
        root,
        ["commit", "--quiet", "-m", "merge with independent patch"],
    )?;
    let unsigned = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    if check_range_with_context(root, &patch_base, &unsigned, &local, false).is_ok() {
        return Err("DCO self-test accepted an unsigned merge-only patch".into());
    }
    git_run(
        root,
        ["commit", "--quiet", "--amend", "--no-edit", "--signoff"],
    )?;
    let signed = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    check_range_with_context(root, &patch_base, &signed, &local, true)
        .map_err(|_| "DCO self-test rejected a signed merge-only patch".to_string())?;

    git_run(root, ["checkout", "--quiet", "-b", "conflict-topic"])?;
    write(root, "shared.txt", "topic conflict\n")?;
    git_run(root, ["add", "shared.txt"])?;
    git_run(
        root,
        ["commit", "--quiet", "--signoff", "-m", "conflict topic"],
    )?;
    git_run(root, ["checkout", "--quiet", "main"])?;
    write(root, "shared.txt", "main conflict\n")?;
    git_run(root, ["add", "shared.txt"])?;
    git_run(
        root,
        ["commit", "--quiet", "--signoff", "-m", "conflict main"],
    )?;
    let conflict_base = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    let conflict = output_allow_failure(Command::new("git").arg("-C").arg(root).args([
        "merge",
        "--quiet",
        "--no-ff",
        "--no-commit",
        "conflict-topic",
    ]))?;
    if conflict.status.success() {
        return Err("DCO self-test expected conflicting parent content".into());
    }
    write(root, "shared.txt", "independent conflict resolution\n")?;
    git_run(root, ["add", "shared.txt"])?;
    git_run(root, ["commit", "--quiet", "-m", "resolve conflict"])?;
    let resolved = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    if check_range_with_context(root, &conflict_base, &resolved, &local, false).is_ok() {
        return Err("DCO self-test accepted an unsigned conflict resolution".into());
    }
    git_run(
        root,
        ["commit", "--quiet", "--amend", "--no-edit", "--signoff"],
    )?;
    let bot_base = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    check_range_with_context(root, &conflict_base, &bot_base, &local, true)
        .map_err(|_| "DCO self-test rejected a signed conflict resolution".to_string())?;

    write(root, "bot.txt", "bot update\n")?;
    git_run(root, ["add", "bot.txt"])?;
    run(Command::new("git")
        .arg("-C")
        .arg(root)
        .env("GIT_AUTHOR_NAME", "dependabot[bot]")
        .env(
            "GIT_AUTHOR_EMAIL",
            "49699333+dependabot[bot]@users.noreply.github.com",
        )
        .env("GIT_COMMITTER_NAME", "GitHub")
        .env("GIT_COMMITTER_EMAIL", "noreply@github.com")
        .args([
            "commit",
            "--quiet",
            "--no-gpg-sign",
            "-m",
            "bot update",
            "-m",
            "Signed-off-by: dependabot[bot] <support@github.com>",
        ]))?;
    let bot_commit = git(root, &["rev-parse", "HEAD"])?.trim().to_string();

    let spoofed_pull_request = CiContext::github(
        "pull_request",
        "human-contributor",
        "dependabot/spoofed",
        "refs/pull/1/merge",
    );
    if check_range_with_context(root, &bot_base, &bot_commit, &spoofed_pull_request, false).is_ok()
    {
        return Err("DCO self-test allowed an ordinary pull request to spoof Dependabot".into());
    }
    let dependabot_pull_request = CiContext::github(
        "pull_request",
        "dependabot[bot]",
        "dependabot/cargo/example",
        "refs/pull/2/merge",
    );
    check_range_with_context(root, &bot_base, &bot_commit, &dependabot_pull_request, true)
        .map_err(|_| "DCO self-test rejected a genuine Dependabot pull request".to_string())?;
    let main_push = CiContext::github("push", "human-merger", "", "refs/heads/main");
    check_range_with_context(root, &bot_base, &bot_commit, &main_push, true)
        .map_err(|_| "DCO self-test rejected a Dependabot commit on main".to_string())?;
    let release_push = CiContext::github("push", "human-merger", "", "refs/heads/release");
    if check_range_with_context(root, &bot_base, &bot_commit, &release_push, false).is_ok() {
        return Err("DCO self-test allowed a Dependabot exemption outside main".into());
    }
    for event in ["schedule", "workflow_dispatch"] {
        let main_run = CiContext::github(event, "human-dispatcher", "", "refs/heads/main");
        check_range_with_context(root, &bot_base, &bot_commit, &main_run, true).map_err(|_| {
            format!("DCO self-test rejected a Dependabot commit in a {event} run on main")
        })?;
        let other_run = CiContext::github(event, "human-dispatcher", "", "refs/heads/release");
        if check_range_with_context(root, &bot_base, &bot_commit, &other_run, false).is_ok() {
            return Err(format!(
                "DCO self-test allowed a Dependabot exemption in a {event} run outside main"
            ));
        }
    }
    let unknown_event = CiContext::github("merge_group", "human-merger", "", "refs/heads/main");
    if check_range_with_context(root, &bot_base, &bot_commit, &unknown_event, false).is_ok() {
        return Err("DCO self-test allowed a Dependabot exemption for an unlisted event".into());
    }

    println!(
        "DCO self-test: merge provenance and narrowly scoped Dependabot PR and main push, schedule and dispatch cases passed."
    );
    Ok(())
}

fn git_run<const N: usize>(root: &Path, args: [&str; N]) -> TaskResult {
    run(Command::new("git").arg("-C").arg(root).args(args))
}

fn write(root: &Path, name: &str, contents: &str) -> TaskResult {
    fs::write(root.join(name), contents)
        .map_err(|error| format!("cannot write self-test file: {error}"))
}
