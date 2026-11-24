//! Check every commit since the last successful main run.
//!
//! Pushes and manual runs use a verified ancestor baseline so a failed earlier
//! run cannot drop commits from later validation. Scheduled runs remain
//! eligible only when they used the same verified range resolver.
use super::{option, receipt, validate_options};
use crate::command::{git, git_is_ancestor, output_allow_failure, repo_root};
use crate::TaskResult;
use serde_json::Value;
use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

/// The repository's name and immutable numeric ID, as the workflow guards
/// pin them: a fork, a copy or a new repository under this name never
/// supplies a base.
const REPOSITORY: &str = "artificial-intelligence-edge/PineWASM";
const REPOSITORY_ID: u64 = 1_470_159_731;
const WORKFLOW: &str = ".github/workflows/ci.yml";

/// The root commit is checked as a complete tree.
/// Subsequent runs check every commit after its unique root until a successful
/// main run supplies a newer baseline.
fn root_baseline(root: &Path) -> Result<String, String> {
    let roots = git(root, &["rev-list", "--max-parents=0", "HEAD"])?;
    let values: Vec<_> = roots.lines().collect();
    if values.len() != 1 {
        return Err("CI requires a single root commit".into());
    }
    Ok(values[0].to_owned())
}

/// A workflow that resolves its base with this command checks every commit
/// since the last passing run. Scheduled and dispatched runs whose workflow
/// lacks this command record no checked range, so their heads never become a base.
const RESOLVER: &str = "pinewasm_xtask -- ci-range-base";

const EVENTS: [&str; 3] = ["schedule", "workflow_dispatch", "push"];
const PER_PAGE: usize = 100;
/// The Actions API lists at most 1,000 runs for a filtered query.
const MAX_PAGES: usize = 10;
const ATTEMPTS: u64 = 3;

/// A successful run whose head can be the base.
#[derive(Debug, PartialEq, Eq)]
struct Checked {
    head: String,
    run_id: u64,
    event: String,
    /// Commits from its head to the commit under test.
    distance: usize,
}

pub(crate) fn range_base(args: &[String]) -> TaskResult {
    validate_options(args, &["--github-output"])?;
    let github_output = option(args, "--github-output")?;
    let event = env::var("GITHUB_EVENT_NAME").unwrap_or_default();
    if !EVENTS.contains(&event.as_str()) {
        return Err(format!(
            "ci-range-base requires a main push or manual run, not {event:?}"
        ));
    }
    if env::var("GITHUB_REF").as_deref() != Ok("refs/heads/main")
        || env::var("GITHUB_REPOSITORY").as_deref() != Ok(REPOSITORY)
        || env::var("GITHUB_REPOSITORY_ID") != Ok(REPOSITORY_ID.to_string())
    {
        return Err(format!("ci-range-base checks main of {REPOSITORY} only"));
    }
    let root = repo_root()?;
    let head = git(&root, &["rev-parse", "--verify", "HEAD^{commit}"])?
        .trim()
        .to_owned();
    if env::var("CI_HEAD_SHA").is_ok_and(|expected| expected != head) {
        return Err("ci-range-base must run in the checkout of the commit under test".into());
    }
    let current = env::var("GITHUB_RUN_ID")
        .ok()
        .and_then(|id| id.parse().ok());
    let floor = root_baseline(&root)?;
    let checked = find(&root, &head, &floor, current, |event, page| {
        listing(&format!(
            "repos/{REPOSITORY}/actions/workflows/ci.yml/runs?branch=main&event={event}&per_page={PER_PAGE}&page={page}"
        ))
    })?;
    let (base, origin) = match &checked {
        Some(run) => (
            run.head.as_str(),
            format!(
                "head of successful {} run https://github.com/{REPOSITORY}/actions/runs/{}",
                run.event, run.run_id
            ),
        ),
        None => (
            floor.as_str(),
            "the root commit; no earlier successful main run checked a range".to_owned(),
        ),
    };
    let range = format!("{base}..{head}");
    let commits = git(&root, &["rev-list", "--count", &range])?;
    let steps = git(&root, &["rev-list", "--count", "--first-parent", &range])?;
    let report = if commits.trim() == "0" {
        format!("No commits since {base}, which already passed; the range checks have nothing new to check.")
    } else {
        format!(
            "The range checks cover {range}: {} commit(s) in {} first-parent step(s).",
            commits.trim(),
            steps.trim()
        )
    };
    println!("Range base {base}: {origin}.");
    println!("{report}");
    if let Some(path) = github_output {
        append(&path, &format!("base={base}\n"))?;
    }
    if let Ok(path) = env::var("GITHUB_STEP_SUMMARY") {
        append(
            &path,
            &format!("### Commit range\n\nBase `{base}`: {origin}.\n\n{report}\n"),
        )?;
    }
    Ok(())
}

fn append(path: &str, text: &str) -> TaskResult {
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(|error| format!("cannot write {path}: {error}"))
}

/// One API listing, retried a few times before the run gives up.
fn listing(endpoint: &str) -> TaskResult<Value> {
    let mut attempt = 1;
    loop {
        match receipt::api(endpoint) {
            Ok(value) => return Ok(value),
            Err(error) if attempt < ATTEMPTS => {
                eprintln!("Range base lookup attempt {attempt} failed; retrying: {error}");
                thread::sleep(Duration::from_secs(10 * attempt));
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

/// The nearest successful run that checked a range, or `None` when no listed
/// run did. A listing that fails, is malformed or cannot be read to its end is
/// an error: the commits to check are then unknown.
fn find(
    root: &Path,
    head: &str,
    floor: &str,
    current: Option<u64>,
    mut list: impl FnMut(&str, usize) -> TaskResult<Value>,
) -> TaskResult<Option<Checked>> {
    if !git_is_ancestor(root, floor, head)? {
        return Err(format!(
            "the pinned range floor {floor} is not in the history of {head}; if main was rewritten, review the unique root and repository identity"
        ));
    }
    let mut best: Option<Checked> = None;
    for event in EVENTS {
        for page in 1..=MAX_PAGES {
            let response = list(event, page).map_err(|error| {
                format!("cannot list {event} runs of {WORKFLOW} on main, so the commits to check are unknown: {error}")
            })?;
            let runs = response["workflow_runs"].as_array().ok_or_else(|| {
                format!(
                    "malformed {event} run listing (page {page}); the commits to check are unknown"
                )
            })?;
            let mut found = false;
            for run in runs {
                match checked(root, head, floor, current, event, run) {
                    Ok(run) => {
                        found = true;
                        if best
                            .as_ref()
                            .is_none_or(|best| run.distance < best.distance)
                        {
                            best = Some(run);
                        }
                    }
                    Err(reason) => eprintln!(
                        "Range base candidate {event} run {} ({}) rejected: {reason}",
                        run["id"],
                        run["head_sha"].as_str().unwrap_or("unknown head")
                    ),
                }
            }
            // Runs are listed newest first and main only moves forward, so a
            // later page holds only older heads.
            if found || runs.len() < PER_PAGE {
                break;
            }
            if page == MAX_PAGES {
                return Err(format!(
                    "none of the newest {} {event} runs passed after checking a range, and the API lists no more; the commits to check are unknown",
                    MAX_PAGES * PER_PAGE
                ));
            }
        }
    }
    Ok(best)
}

/// Accepts a run only if it passed every check over a range that ends at its
/// head, and that head lies between the floor and the commit under test.
fn checked(
    root: &Path,
    head: &str,
    floor: &str,
    current: Option<u64>,
    event: &str,
    run: &Value,
) -> Result<Checked, String> {
    let run_id = run["id"].as_u64().ok_or("malformed run record")?;
    if Some(run_id) == current {
        return Err("this run".into());
    }
    if run["status"] != "completed" || run["conclusion"] != "success" {
        return Err(format!(
            "not successful (status {}, conclusion {})",
            run["status"], run["conclusion"]
        ));
    }
    if run["event"] != event
        || run["head_branch"] != "main"
        || run["path"] != WORKFLOW
        || run["repository"]["full_name"] != REPOSITORY
        || run["repository"]["id"] != REPOSITORY_ID
        || run["head_repository"]["full_name"] != REPOSITORY
        || run["head_repository"]["id"] != REPOSITORY_ID
    {
        return Err(format!(
            "not a {event} run of {WORKFLOW} on main of {REPOSITORY}"
        ));
    }
    let sha = run["head_sha"]
        .as_str()
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or("malformed head commit")?;
    let known = output_allow_failure(Command::new("git").arg("-C").arg(root).args([
        "cat-file",
        "-e",
        &format!("{sha}^{{commit}}"),
    ]))?;
    if !known.status.success() {
        return Err("head commit is not in this history".into());
    }
    if !git_is_ancestor(root, sha, head)? {
        return Err("head is not an ancestor of the commit under test".into());
    }
    if !git_is_ancestor(root, floor, sha)? {
        return Err("head precedes the pinned floor".into());
    }
    let workflow = git(root, &["show", &format!("{sha}:{WORKFLOW}")])
        .map_err(|_| format!("no {WORKFLOW} at its head"))?;
    if !workflow.contains(RESOLVER) {
        return Err("its workflow did not use the verified commit-range resolver".into());
    }
    let distance = git(root, &["rev-list", "--count", &format!("{sha}..{head}")])?
        .trim()
        .parse()
        .map_err(|error| format!("cannot count commits after its head: {error}"))?;
    Ok(Checked {
        head: sha.to_owned(),
        run_id,
        event: event.to_owned(),
        distance,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{temp_dir, TempDir};
    use serde_json::json;
    use std::cell::Cell;

    const LEGACY: &str = "on: schedule\n# An empty range on schedule and dispatch.\n";
    const CURRENT: &str =
        "on: schedule\n# run: cargo run --locked -p pinewasm_xtask -- ci-range-base\n";

    #[test]
    fn range_resolver_uses_the_workflows_repository_id() {
        let workflow = include_str!("../../../../.github/workflows/ci.yml");
        assert!(workflow.contains(&format!("github.repository_id == '{REPOSITORY_ID}'")));
    }

    struct Repo(TempDir);

    impl Repo {
        fn new() -> Self {
            let repo = Self(temp_dir("ci-range-base").unwrap());
            repo.git(&["init", "--quiet"]);
            repo.git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
            repo.git(&["config", "user.name", "Range Test"]);
            repo.git(&["config", "user.email", "range-test@example.invalid"]);
            repo.git(&["config", "commit.gpgsign", "false"]);
            repo.git(&["config", "core.hooksPath", "no-hooks"]);
            repo
        }

        fn root(&self) -> &Path {
            self.0.path()
        }

        fn git(&self, args: &[&str]) -> String {
            git(self.root(), args).unwrap().trim().to_owned()
        }

        fn commit(&self, workflow: &str, message: &str) -> String {
            let path = self.root().join(WORKFLOW);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, format!("{workflow}# {message}\n")).unwrap();
            self.git(&["add", "--all"]);
            self.git(&["commit", "--quiet", "-m", message]);
            self.git(&["rev-parse", "HEAD"])
        }
    }

    fn record(id: u64, event: &str, head: &str, conclusion: &str) -> Value {
        json!({
            "id": id,
            "event": event,
            "status": "completed",
            "conclusion": conclusion,
            "head_branch": "main",
            "path": WORKFLOW,
            "head_sha": head,
            "repository": {"full_name": REPOSITORY, "id": REPOSITORY_ID},
            "head_repository": {"full_name": REPOSITORY, "id": REPOSITORY_ID}
        })
    }

    /// One page of runs per event.
    fn listed(
        schedule: Vec<Value>,
        dispatch: Vec<Value>,
    ) -> impl FnMut(&str, usize) -> TaskResult<Value> {
        move |event: &str, page: usize| {
            let runs = match (event, page) {
                ("schedule", 1) => schedule.clone(),
                ("workflow_dispatch", 1) => dispatch.clone(),
                _ => Vec::new(),
            };
            Ok(json!({ "workflow_runs": runs }))
        }
    }

    #[test]
    fn fresh_history_uses_its_unique_root() {
        let repo = Repo::new();
        let first = repo.commit(CURRENT, "initial snapshot");
        repo.commit(CURRENT, "next commit");
        assert_eq!(root_baseline(repo.root()).unwrap(), first);
        repo.git(&["checkout", "--orphan", "unrelated"]);
        repo.commit(CURRENT, "unrelated root");
        repo.git(&[
            "merge",
            "--allow-unrelated-histories",
            "-s",
            "ours",
            "--no-edit",
            "main",
        ]);
        assert!(root_baseline(repo.root()).is_err());
    }

    #[test]
    fn the_nearest_passing_run_that_checked_a_range_is_the_base() {
        let repo = Repo::new();
        let root = repo.root();
        let floor = repo.commit(LEGACY, "floor");
        let legacy = repo.commit(LEGACY, "legacy weekly run");
        let checked_once = repo.commit(CURRENT, "first run that checks ranges");
        let newer = repo.commit(CURRENT, "newer");
        let head = repo.commit(CURRENT, "head");
        let schedule = vec![
            record(4, "schedule", &newer, "failure"),
            record(3, "schedule", &checked_once, "success"),
            record(2, "schedule", &legacy, "success"),
        ];
        let base = find(root, &head, &floor, None, listed(schedule.clone(), vec![]))
            .unwrap()
            .unwrap();
        assert_eq!(
            base,
            Checked {
                head: checked_once.clone(),
                run_id: 3,
                event: "schedule".into(),
                distance: 2
            }
        );
        // A later dispatched run on main moves the base forward.
        let dispatch = vec![record(5, "workflow_dispatch", &newer, "success")];
        let base = find(root, &head, &floor, None, listed(schedule, dispatch))
            .unwrap()
            .unwrap();
        assert_eq!((base.head, base.run_id, base.distance), (newer, 5, 1));
        // A run on the commit under test leaves nothing new to check.
        let same = vec![record(6, "workflow_dispatch", &head, "success")];
        let base = find(root, &head, &floor, None, listed(vec![], same))
            .unwrap()
            .unwrap();
        assert_eq!((base.head, base.distance), (head, 0));
    }

    #[test]
    fn runs_that_checked_no_range_leave_the_floor_as_the_base() {
        let repo = Repo::new();
        let root = repo.root();
        let older = repo.commit(LEGACY, "before the floor");
        let floor = repo.commit(LEGACY, "floor");
        let legacy = repo.commit(LEGACY, "legacy weekly run");
        let head = repo.commit(CURRENT, "head");
        let schedule = vec![record(2, "schedule", &legacy, "success")];
        let dispatch = vec![
            record(1, "workflow_dispatch", &floor, "success"),
            record(0, "workflow_dispatch", &older, "success"),
        ];
        assert_eq!(
            find(root, &head, &floor, None, listed(schedule, dispatch)),
            Ok(None)
        );
        assert_eq!(
            find(root, &head, &floor, None, listed(vec![], vec![])),
            Ok(None)
        );
    }

    #[test]
    fn a_lookup_that_cannot_finish_fails_closed() {
        let repo = Repo::new();
        let root = repo.root();
        let floor = repo.commit(LEGACY, "floor");
        let head = repo.commit(CURRENT, "head");
        let unavailable = |_: &str, _: usize| -> TaskResult<Value> { Err("HTTP 502".into()) };
        let error = find(root, &head, &floor, None, unavailable).unwrap_err();
        assert!(error.contains("commits to check are unknown"), "{error}");
        let malformed = |_: &str, _: usize| Ok(json!({"message": "Bad credentials"}));
        assert!(find(root, &head, &floor, None, malformed).is_err());
        // Full pages of failed runs until the API stops listing more.
        let failing = |_: &str, _: usize| {
            Ok(json!({"workflow_runs": vec![record(9, "schedule", &head, "failure"); PER_PAGE]}))
        };
        let error = find(root, &head, &floor, None, failing).unwrap_err();
        assert!(
            error.contains("none of the newest 1000 schedule runs"),
            "{error}"
        );
        // A floor outside the history (main was rewritten) is no base.
        repo.git(&["checkout", "--quiet", "-b", "rewritten", &floor]);
        let rewritten = repo.commit(LEGACY, "rewritten floor");
        let error = find(root, &head, &rewritten, None, listed(vec![], vec![])).unwrap_err();
        assert!(error.contains("is not in the history"), "{error}");
    }

    #[test]
    fn paging_stops_once_a_page_holds_a_checked_run() {
        let repo = Repo::new();
        let root = repo.root();
        let floor = repo.commit(LEGACY, "floor");
        let checked_once = repo.commit(CURRENT, "checked");
        let head = repo.commit(CURRENT, "head");
        let calls = Cell::new(0);
        let pages = |event: &str, page: usize| {
            calls.set(calls.get() + 1);
            let runs = match (event, page) {
                ("schedule", 1) => vec![record(8, "schedule", &head, "failure"); PER_PAGE],
                ("schedule", 2) => vec![record(7, "schedule", &checked_once, "success")],
                ("schedule", _) => panic!("read past the first page with a checked run"),
                _ => Vec::new(),
            };
            Ok(json!({ "workflow_runs": runs }))
        };
        let base = find(root, &head, &floor, None, pages).unwrap().unwrap();
        assert_eq!((base.head, base.run_id), (checked_once, 7));
        assert_eq!(calls.get(), EVENTS.len() + 1);
    }

    #[test]
    fn successful_push_advances_the_base_and_failed_push_does_not() {
        let repo = Repo::new();
        let root = repo.root();
        let floor = repo.commit(CURRENT, "root");
        let passed = repo.commit(CURRENT, "passed push");
        let failed = repo.commit(CURRENT, "failed push");
        let head = repo.commit(CURRENT, "current push");
        let base = find(root, &head, &floor, Some(4), |event, page| {
            let runs = if event == "push" && page == 1 {
                vec![
                    record(4, "push", &head, "success"),
                    record(3, "push", &failed, "failure"),
                    record(2, "push", &passed, "success"),
                ]
            } else {
                Vec::new()
            };
            Ok(json!({"workflow_runs": runs}))
        })
        .unwrap()
        .unwrap();
        assert_eq!((base.head, base.event), (passed, "push".into()));
    }

    #[test]
    fn foreign_unfinished_and_unrelated_runs_are_rejected() {
        let repo = Repo::new();
        let root = repo.root();
        let older = repo.commit(LEGACY, "before the floor");
        let floor = repo.commit(LEGACY, "floor");
        let base = repo.commit(CURRENT, "checked");
        let head = repo.commit(CURRENT, "head");
        repo.git(&["checkout", "--quiet", "-b", "side", &base]);
        let side = repo.commit(CURRENT, "never merged");
        let good = record(11, "schedule", &base, "success");
        let check = |run: &Value| checked(root, &head, &floor, Some(12), "schedule", run);
        assert_eq!(check(&good).map(|run| run.distance), Ok(1));
        let mut cases = Vec::new();
        for (field, value) in [
            ("conclusion", json!("cancelled")),
            ("status", json!("in_progress")),
            ("event", json!("pull_request")),
            ("event", json!("workflow_dispatch")),
            ("head_branch", json!("feature")),
            ("path", json!(".github/workflows/other.yml")),
            (
                "repository",
                json!({"full_name": "someone/PineWASM", "id": REPOSITORY_ID}),
            ),
            ("repository", json!({"full_name": REPOSITORY, "id": 1})),
            ("repository", json!({"full_name": REPOSITORY})),
            (
                "head_repository",
                json!({"full_name": "someone/PineWASM", "id": REPOSITORY_ID}),
            ),
            ("head_repository", json!({"full_name": REPOSITORY, "id": 1})),
            ("head_repository", json!({"full_name": REPOSITORY})),
            ("head_sha", json!(&base[..12])),
            ("head_sha", json!("f".repeat(40))),
            ("head_sha", json!(side)),
            ("head_sha", json!(older)),
            ("id", json!(12)),
            ("id", json!("11")),
        ] {
            let mut run = good.clone();
            run[field] = value;
            cases.push(run);
        }
        for run in &cases {
            assert!(check(run).is_err(), "{run}");
        }
    }
}
