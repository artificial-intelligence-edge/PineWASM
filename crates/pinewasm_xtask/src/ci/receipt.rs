//! Success records are stored as immutable CI artifacts, never build caches.
use super::{graph::Graph, option, read_plan, validate_options, Plan, SCHEMA};
use crate::command::{git, git_bytes, output, pinned_toolchain, repo_root, temp_dir};
use crate::TaskResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: u32,
    lane: String,
    fingerprint: String,
    run_id: String,
    run_attempt: String,
    head_sha: String,
    tested_commit: String,
    repository: String,
}

const LANES: &[&str] = &[
    "rust-linux-x64",
    "rust-macos-arm64",
    "rust-windows-x64",
    "msrv",
    "adversarial",
    "aot-runtime",
    "wasm",
];

fn lane(args: &[String]) -> TaskResult<String> {
    validate_options(args, &["--lane", "--output", "--github-output"])?;
    let lane = option(args, "--lane")?.ok_or("CI command requires --lane")?;
    if !LANES.contains(&lane.as_str()) {
        return Err(format!("unknown CI lane {lane}"));
    }
    Ok(lane)
}

fn fingerprint(root: &Path, plan: &Plan, lane: &str) -> TaskResult<String> {
    fingerprint_at(root, plan, lane, "HEAD")
}

fn fingerprint_at(root: &Path, plan: &Plan, lane: &str, revision: &str) -> TaskResult<String> {
    let graph = Graph::read(root)?;
    let selected = if lane == "adversarial" || plan.docs {
        // These recipes inspect the whole workspace even when source selection is narrow.
        graph.packages.keys().cloned().collect()
    } else if matches!(lane, "wasm" | "aot-runtime") {
        BTreeSet::from(["pinewasm_bindings".to_owned(), "pinewasm_aot".to_owned()])
    } else {
        plan.packages.keys().cloned().collect()
    };
    let dependencies = graph.dependencies(&selected);
    let directories = dependencies
        .iter()
        .filter_map(|name| graph.packages.get(name))
        .map(|p| format!("{}/", p.directory))
        .collect::<Vec<_>>();
    let mut hash = Sha256::new();
    hash.update(format!(
        "pinewasm-ci-{SCHEMA}\0{lane}\0{}\0{}\0{}\0",
        env::consts::OS,
        env::consts::ARCH,
        pinned_toolchain(root)?
    ));
    hash.update(
        serde_json::to_vec(&(
            &plan.packages,
            plan.full,
            plan.docs,
            plan.runtime,
            &plan.runtime_variants,
            plan.adversarial,
        ))
        .map_err(|e| e.to_string())?,
    );
    for name in [
        "PINEWASM_RUST_MSRV",
        "PINEWASM_VALIDATE_PASSES",
        "RUSTFLAGS",
        "RUSTDOCFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_TARGET",
        "PROPTEST_CASES",
        "PROPTEST_RNG_SEED",
    ] {
        hash.update(format!("\0{name}={}\0", env::var(name).unwrap_or_default()));
    }
    // Git object identities bind bytes AND paths/modes. Unknown non-crate inputs
    // are deliberately included rather than optimistically excluded.
    let mut entries = BTreeMap::new();
    for entry in tree_bytes(root, revision)?
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
    {
        let text = std::str::from_utf8(entry).map_err(|e| e.to_string())?;
        let (_, path) = text.split_once('\t').ok_or("malformed Git tree")?;
        if path.ends_with("Cargo.toml")
            || !path.starts_with("crates/")
            || path.starts_with("crates/pinewasm_xtask/")
            || directories.iter().any(|dir| path.starts_with(dir))
        {
            entries.insert(path.to_owned(), entry.to_vec());
        }
    }
    for entry in entries.values() {
        hash.update(entry);
        hash.update([0]);
    }
    // Package identity and repository-inspecting xtask tests intentionally depend
    // on the exact commit. Their results cannot migrate across a merge SHA.
    if lane == "wasm" || plan.packages.contains_key("pinewasm_xtask") {
        hash.update(&plan.candidate);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn artifact_name(lane: &str, key: &str) -> String {
    format!("pinewasm-ci-v{SCHEMA}-{lane}-{key}")
}

pub(crate) fn record(args: &[String]) -> TaskResult {
    let lane = lane(args)?;
    let root = repo_root()?;
    let plan = read_plan(&root)?;
    if !git(&root, &["diff", "HEAD", "--name-only"])?
        .trim()
        .is_empty()
    {
        return Err("cannot record success from a modified candidate tree".into());
    }
    let key = fingerprint(&root, &plan, &lane)?;
    let record = Receipt {
        schema: SCHEMA,
        lane: lane.clone(),
        fingerprint: key.clone(),
        run_id: env::var("GITHUB_RUN_ID").map_err(|_| "success records require a CI producer")?,
        run_attempt: env::var("GITHUB_RUN_ATTEMPT").map_err(|_| "missing run attempt")?,
        head_sha: env::var("CI_HEAD_SHA").map_err(|_| "missing CI_HEAD_SHA")?,
        tested_commit: plan.candidate,
        repository: env::var("GITHUB_REPOSITORY").map_err(|_| "missing repository")?,
    };
    let path = option(args, "--output")?.ok_or("ci-record requires --output")?;
    let encoded = serde_json::to_string(&record).map_err(|e| e.to_string())?;
    fs::write(path, &encoded).map_err(|e| e.to_string())?;
    println!("PINEWASM_CI_RECEIPT {encoded}");
    github_output(args, "artifact", &artifact_name(&lane, &key))?;
    Ok(())
}

pub(crate) fn reuse(args: &[String]) -> TaskResult {
    let lane = lane(args)?;
    let root = repo_root()?;
    let plan = read_plan(&root)?;
    let key = fingerprint(&root, &plan, &lane)?;
    // Absence, API errors, expired artifacts or invalid records mean execute.
    // A transport failure must never turn a required test into a skipped job.
    let reused = if env::var("CI_DISABLE_REUSE").as_deref() == Ok("true") {
        false
    } else {
        match find_success(&root, &plan, &lane, &key) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("CI success reuse unavailable; executing checks: {error}");
                false
            }
        }
    };
    github_output(args, "reused", &reused.to_string())?;
    println!(
        "{lane}: {}",
        if reused {
            "reused verified successful Actions run"
        } else {
            "execute selected checks"
        }
    );
    Ok(())
}

fn github_output(args: &[String], key: &str, value: &str) -> TaskResult {
    if let Some(path) = option(args, "--github-output")? {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        writeln!(file, "{key}={value}").map_err(|e| e.to_string())?;
    }
    Ok(())
}

// Use curl, which runner bootstrap already requires; the GitHub CLI is not
// guaranteed on every runner image. Send credentials through stdin, never
// command args.
fn request(endpoint: &str, accept: &str, limit: &str) -> TaskResult<Vec<u8>> {
    let token = env::var("GH_TOKEN").map_err(|_| "missing scoped Actions token")?;
    if token.contains(['\n', '\r']) {
        return Err("invalid scoped Actions token".into());
    }
    let mut child = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "45",
            "--max-filesize",
            limit,
            "--header",
            "@-",
            "--url",
        ])
        .arg(format!("https://api.github.com/{endpoint}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot query Actions: {e}"))?;
    {
        let mut input = child.stdin.take().ok_or("missing request input")?;
        write!(
            input,
            "Authorization: Bearer {token}\nAccept: {accept}\nX-GitHub-Api-Version: 2022-11-28\n"
        )
        .map_err(|e| e.to_string())?;
    }
    let response = child.wait_with_output().map_err(|e| e.to_string())?;
    if !response.status.success() {
        return Err(format!(
            "Actions request failed: {}",
            String::from_utf8_lossy(&response.stderr)
        ));
    }
    Ok(response.stdout)
}

pub(super) fn api(endpoint: &str) -> TaskResult<Value> {
    serde_json::from_slice(&request(
        endpoint,
        "application/vnd.github+json",
        "20000000",
    )?)
    .map_err(|e| e.to_string())
}

fn find_success(root: &Path, plan: &Plan, lane: &str, key: &str) -> TaskResult<bool> {
    if find_artifact_success(root, plan, lane, key).unwrap_or(false) {
        return Ok(true);
    }
    find_logged_success(root, plan, lane, key)
}

fn receipt_from_log(line: &str) -> Option<Receipt> {
    let (_, json) = line.split_once("PINEWASM_CI_RECEIPT ")?;
    if json.len() > 16384 {
        return None;
    }
    serde_json::from_str(json).ok()
}

fn find_logged_success(root: &Path, plan: &Plan, lane: &str, key: &str) -> TaskResult<bool> {
    let repository = env::var("GITHUB_REPOSITORY").map_err(|_| "missing repository")?;
    if repository != "artificial-intelligence-edge/PineWASM" {
        return Ok(false);
    }
    // Bound lookup to this revision and its direct integration parents. This
    // covers retries and repeated runs of one revision without downloading unrelated logs.
    let revisions = git(root, &["rev-list", "--parents", "-n", "1", "HEAD"])?;
    let mut revisions = revisions
        .split_whitespace()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if let Ok(head) = env::var("CI_HEAD_SHA") {
        revisions.insert(head);
    }
    let expected_job = match lane {
        "rust-linux-x64" => "Rust quality and workspace tests (linux-x64)",
        "rust-macos-arm64" => "Rust quality and workspace tests (macos-arm64)",
        "rust-windows-x64" => "Rust quality and workspace tests (windows-x64)",
        "msrv" => "Declared MSRV floor still builds",
        "adversarial" => "Deterministic adversarial smoke",
        "aot-runtime" => "Checked-in AOT runtimes are reproducible",
        "wasm" => "Production WASM package",
        _ => return Ok(false),
    };
    for revision in revisions {
        if revision.len() != 40 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let runs = api(&format!("repos/{repository}/actions/workflows/ci.yml/runs?status=success&head_sha={revision}&per_page=5"))?;
        for run in runs["workflow_runs"].as_array().ok_or("missing runs")? {
            if !eligible_run(run) {
                continue;
            }
            // Cheap content prefilter before requesting a potentially large log.
            if fingerprint_at(root, plan, lane, &revision).ok().as_deref() != Some(key) {
                continue;
            }
            let Some(run_id) = run["id"].as_u64() else {
                continue;
            };
            let jobs = api(&format!(
                "repos/{repository}/actions/runs/{run_id}/jobs?per_page=100"
            ))?;
            for job in jobs["jobs"].as_array().ok_or("missing jobs")? {
                if job["name"] != expected_job
                    || job["conclusion"] != "success"
                    || job["status"] != "completed"
                {
                    continue;
                }
                let Some(job_id) = job["id"].as_u64() else {
                    continue;
                };
                let log = request(
                    &format!("repos/{repository}/actions/jobs/{job_id}/logs"),
                    "application/vnd.github+json",
                    "20000000",
                )?;
                for receipt in String::from_utf8_lossy(&log)
                    .lines()
                    .filter_map(receipt_from_log)
                {
                    if valid_receipt(&receipt, run, lane, key, &repository, run_id)
                        && verified_inputs(root, plan, lane, key, &receipt)
                    {
                        println!("Evidence: https://github.com/{repository}/actions/runs/{run_id}/job/{job_id} ({lane}, verified job log)");
                        return Ok(true);
                    }
                }
            }
        }
    }
    Ok(false)
}

fn verified_inputs(root: &Path, plan: &Plan, lane: &str, key: &str, receipt: &Receipt) -> bool {
    let producer = workflow_source(root, &receipt.tested_commit);
    producer.is_ok()
        && producer.as_ref().ok()
            == fs::read_to_string(root.join(".github/workflows/ci.yml"))
                .as_ref()
                .ok()
        && fingerprint_at(root, plan, lane, &receipt.tested_commit)
            .ok()
            .as_deref()
            == Some(key)
        && (lane != "wasm" || receipt.tested_commit == plan.candidate)
}

fn find_artifact_success(root: &Path, plan: &Plan, lane: &str, key: &str) -> TaskResult<bool> {
    let repository = env::var("GITHUB_REPOSITORY").map_err(|_| "no GitHub repository")?;
    if repository != "artificial-intelligence-edge/PineWASM" {
        return Ok(false);
    }
    let artifact = artifact_name(lane, key);
    let listing = api(&format!(
        "repos/{repository}/actions/artifacts?name={artifact}&per_page=20"
    ))?;
    for item in listing["artifacts"]
        .as_array()
        .ok_or("missing artifact list")?
    {
        if item["expired"] != false || item["name"] != artifact {
            continue;
        }
        let Some(run_id) = item["workflow_run"]["id"].as_u64() else {
            continue;
        };
        let run = api(&format!("repos/{repository}/actions/runs/{run_id}"))?;
        if !eligible_run(&run) {
            continue;
        }
        let directory = temp_dir("ci-success-record")?;
        let Some(artifact_id) = item["id"].as_u64() else {
            continue;
        };
        let archive = directory.path().join("receipt.zip");
        let download = request(
            &format!("repos/{repository}/actions/artifacts/{artifact_id}/zip"),
            "application/vnd.github+json",
            "65536",
        );
        let Ok(download) = download else {
            continue;
        };
        fs::write(&archive, download).map_err(|e| e.to_string())?;
        // Never extract untrusted paths. Check the uncompressed size before
        // reading the sole expected JSON member. Missing unzip means a miss.
        let listing = output(Command::new("unzip").arg("-l").arg(&archive))?;
        let sizes = String::from_utf8_lossy(&listing.stdout)
            .lines()
            .filter_map(|line| {
                let fields = line.split_whitespace().collect::<Vec<_>>();
                (fields.len() == 4 && fields[3] == "receipt.json")
                    .then(|| fields[0].parse::<usize>().ok())
                    .flatten()
            })
            .collect::<Vec<_>>();
        if sizes.len() != 1 || sizes[0] > 16384 {
            continue;
        }
        let bytes = output(
            Command::new("unzip")
                .arg("-p")
                .arg(&archive)
                .arg("receipt.json"),
        )?
        .stdout;
        if bytes.len() > 16384 {
            continue;
        }
        let Ok(receipt) = serde_json::from_slice::<Receipt>(&bytes) else {
            continue;
        };
        if !valid_receipt(&receipt, &run, lane, key, &repository, run_id) {
            continue;
        }
        if !verified_inputs(root, plan, lane, key, &receipt) {
            continue;
        }
        println!("Evidence: https://github.com/{repository}/actions/runs/{run_id} ({lane})");
        return Ok(true);
    }
    Ok(false)
}

fn eligible_run(run: &Value) -> bool {
    run["status"] == "completed"
        && run["conclusion"] == "success"
        && run["path"] == ".github/workflows/ci.yml"
        && run["head_repository"]["full_name"] == "artificial-intelligence-edge/PineWASM"
        && matches!(
            run["event"].as_str(),
            Some("push" | "pull_request" | "schedule" | "workflow_dispatch")
        )
}

fn valid_receipt(
    receipt: &Receipt,
    run: &Value,
    lane: &str,
    key: &str,
    repository: &str,
    run_id: u64,
) -> bool {
    receipt.schema == SCHEMA
        && receipt.lane == lane
        && receipt.fingerprint == key
        && receipt.repository == repository
        && receipt.run_id == run_id.to_string()
        && receipt.head_sha == run["head_sha"].as_str().unwrap_or_default()
        && receipt.run_attempt == run["run_attempt"].as_u64().unwrap_or_default().to_string()
        && receipt.tested_commit.len() == 40
        && receipt.tested_commit.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A successful tested tree can seed omissions only once that exact tree occurs
/// in this candidate's ancestry. For a PR run, the logged candidate must still
/// be that PR head or its merge candidate, whose parent is the PR head. A squash
/// commit need not retain the PR head SHA when the exact validated tree is present.
/// A PR tree not present in candidate history, or a merely similar tree, cannot
/// become a baseline.
pub(super) fn validated_baseline(root: &Path, candidate: &str) -> TaskResult<Option<String>> {
    let repository = env::var("GITHUB_REPOSITORY").map_err(|_| "missing repository")?;
    if repository != "artificial-intelligence-edge/PineWASM" {
        return Ok(None);
    }
    let history = git(
        root,
        &["log", "--format=%H %T", "--max-count=512", candidate],
    )?;
    let ancestors = history
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(commit, tree)| (commit.to_owned(), tree.to_owned()))
        .collect::<Vec<_>>();
    let parents = |revision: &str| {
        git(root, &["rev-list", "--parents", "-n", "1", revision])
            .map(|line| {
                line.split_whitespace()
                    .skip(1)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let base = env::var("CI_BASE_SHA").unwrap_or_default();
    // A pull request's base merge carries the merged PR head as its second
    // parent; that PR run often tested the identical tree before main did.
    let base_merged = parents(&base).into_iter().skip(1);
    let direct = direct_candidates(
        std::iter::once(base.clone())
            .chain(parents(candidate))
            .chain(base_merged),
    );
    let mut search = BaselineSearch {
        root,
        repository: &repository,
        ancestors: &ancestors,
        seen: BTreeSet::new(),
        best: None,
    };
    // Ask for the base and the candidate's parents by head SHA, unfiltered,
    // then filter the returned runs here.
    for sha in &direct {
        let runs = match api(&format!(
            "repos/{repository}/actions/workflows/ci.yml/runs?head_sha={sha}&per_page=10"
        )) {
            Ok(runs) => runs,
            Err(error) => {
                eprintln!("Baseline lookup: runs for {sha} unavailable: {error}");
                continue;
            }
        };
        let runs = successful_runs(&runs);
        eprintln!(
            "Baseline lookup: head_sha {sha} returned {} successful run(s)",
            runs.len()
        );
        for run in runs {
            search.consider(run);
        }
        if search.best == Some(0) {
            return Ok(search.baseline());
        }
    }
    let runs = match api(&format!(
        "repos/{repository}/actions/workflows/ci.yml/runs?status=success&per_page=30"
    )) {
        Ok(runs) => runs,
        // A direct baseline stands on its own; without one the error decides.
        Err(error) if search.best.is_some() => {
            eprintln!("Baseline lookup: successful run list unavailable: {error}");
            return Ok(search.baseline());
        }
        Err(error) => return Err(error),
    };
    let runs = runs["workflow_runs"]
        .as_array()
        .ok_or("missing workflow runs")?;
    eprintln!(
        "Baseline lookup: successful run list returned {} run(s)",
        runs.len()
    );
    for run in runs {
        search.consider(run);
        if search.best == Some(0) {
            break;
        }
    }
    Ok(search.baseline())
}

/// The base, the candidate's parents (the merged PR head is the second parent
/// of a merge commit) and the base's merged PR head, in that order: valid
/// object IDs only, each once, at most three lookups.
fn direct_candidates(shas: impl IntoIterator<Item = String>) -> Vec<String> {
    let oid = |value: &str| value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit());
    let mut result = Vec::new();
    for sha in shas {
        if result.len() < 3 && oid(&sha) && !result.contains(&sha) {
            result.push(sha);
        }
    }
    result
}

fn successful_runs(page: &Value) -> Vec<&Value> {
    page["workflow_runs"]
        .as_array()
        .map(|runs| {
            runs.iter()
                .filter(|run| run["status"] == "completed" && run["conclusion"] == "success")
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rejection {
    Malformed,
    NotEligible,
    HeadNotAncestor,
    JobsLookupFailed,
    NoResultJob,
    LogFetchFailed,
    NoValidatedPlan,
    WorkflowLacksVerify,
    CommitIdentityFailed,
    TreeNotInAncestry,
}

impl Rejection {
    fn reason(self) -> &'static str {
        match self {
            Self::Malformed => "malformed run record",
            Self::NotEligible => "not eligible",
            Self::HeadNotAncestor => "head not an ancestor",
            Self::JobsLookupFailed => "jobs lookup failed",
            Self::NoResultJob => "no successful `CI result` job",
            Self::LogFetchFailed => "log fetch failed",
            Self::NoValidatedPlan => "no validated plan in log",
            Self::WorkflowLacksVerify => "workflow lacks ci-verify",
            Self::CommitIdentityFailed => "commit identity failed",
            Self::TreeNotInAncestry => "tree not in ancestry",
        }
    }
}

struct BaselineSearch<'a> {
    root: &'a Path,
    repository: &'a str,
    ancestors: &'a [(String, String)],
    seen: BTreeSet<u64>,
    best: Option<usize>,
}

impl BaselineSearch<'_> {
    /// Checks each run once, whichever query returned it; the nearest
    /// identical ancestor tree wins.
    fn consider(&mut self, run: &Value) {
        let Some(run_id) = run["id"].as_u64() else {
            eprintln!(
                "Baseline candidate rejected: {}",
                Rejection::Malformed.reason()
            );
            return;
        };
        if !self.seen.insert(run_id) {
            return;
        }
        match self.check(run, run_id) {
            Ok(index) => {
                if self.best.is_none_or(|previous| index < previous) {
                    self.best = Some(index);
                    eprintln!(
                        "Validated baseline {} from successful CI run {run_id} (identical tested tree)",
                        self.ancestors[index].0
                    );
                }
            }
            Err(rejection) => eprintln!(
                "Baseline candidate run {run_id} ({}) rejected: {}",
                run["head_sha"].as_str().unwrap_or("unknown head"),
                rejection.reason()
            ),
        }
    }

    fn check(&self, run: &Value, run_id: u64) -> Result<usize, Rejection> {
        let (root, repository, ancestors) = (self.root, self.repository, self.ancestors);
        if !eligible_run(run) {
            return Err(Rejection::NotEligible);
        }
        if !run_head_passes_preliminary_gate(run, ancestors) {
            return Err(Rejection::HeadNotAncestor);
        }
        let attempt = run["run_attempt"].as_u64().unwrap_or_default();
        // Read the exact successful attempt, not the mutable latest-attempt view.
        let jobs = api(&format!(
            "repos/{repository}/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100"
        ))
        .map_err(|_| Rejection::JobsLookupFailed)?;
        let job_id = jobs["jobs"]
            .as_array()
            .and_then(|jobs| jobs.iter().find(|j| successful_result_job(j)))
            .and_then(|job| job["id"].as_u64())
            .ok_or(Rejection::NoResultJob)?;
        let log = request(
            &format!("repos/{repository}/actions/jobs/{job_id}/logs"),
            "application/vnd.github+json",
            "2000000",
        )
        .map_err(|_| Rejection::LogFetchFailed)?;
        let plan = validated_plan_from_log(&String::from_utf8_lossy(&log))
            .ok_or(Rejection::NoValidatedPlan)?;
        let workflow =
            workflow_source(root, &plan.candidate).map_err(|_| Rejection::WorkflowLacksVerify)?;
        if !workflow.contains("ci-verify") || !workflow.contains("runtime-safety:") {
            return Err(Rejection::WorkflowLacksVerify);
        }
        // Bind the logged snapshot to immutable Git data and the run's head.
        let (tree, parents) =
            commit_identity(root, &plan.candidate).map_err(|_| Rejection::CommitIdentityFailed)?;
        integrated_snapshot(&plan, run, &tree, &parents, ancestors)
            .ok_or(Rejection::TreeNotInAncestry)
    }

    fn baseline(&self) -> Option<String> {
        self.best.map(|index| self.ancestors[index].0.clone())
    }
}

fn successful_result_job(job: &Value) -> bool {
    job["name"] == "CI result" && job["status"] == "completed" && job["conclusion"] == "success"
}

/// Push and dispatch runs must have their head in candidate history before we
/// fetch their logs. A pull-request head may be absent after squash merging, so
/// let that run reach `integrated_snapshot`, which requires the validated plan
/// to bind the head and its exact tested tree to candidate history.
fn run_head_passes_preliminary_gate(run: &Value, ancestors: &[(String, String)]) -> bool {
    let Some(head) = run["head_sha"].as_str() else {
        return false;
    };
    run["event"] == "pull_request" || ancestors.iter().any(|(sha, _)| sha == head)
}

fn validated_plan_from_log(log: &str) -> Option<Plan> {
    // The job-level environment prints the plan before repository code runs.
    // An explicit completion record, when present, takes precedence.
    let explicit = log
        .lines()
        .filter_map(|line| {
            line.split_once("PINEWASM_CI_VALIDATED_PLAN ")
                .map(|(_, value)| value)
        })
        .next_back();
    let encoded = explicit.or_else(|| {
        log.lines()
            .find_map(|line| line.split_once("CI_PLAN: ").map(|(_, value)| value))
    })?;
    if encoded.len() > 131072 {
        return None;
    }
    let plan: Plan = serde_json::from_str(encoded).ok()?;
    let oid = |value: &str| value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit());
    (plan.schema == SCHEMA && oid(&plan.candidate) && oid(&plan.tree)).then_some(plan)
}

fn commit_identity(root: &Path, revision: &str) -> TaskResult<(String, Vec<String>)> {
    if let Ok(value) = git(root, &["show", "-s", "--format=%T%n%P", revision]) {
        let mut lines = value.lines();
        let tree = lines.next().ok_or("missing commit tree")?.to_owned();
        let parents = lines
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        return Ok((tree, parents));
    }
    let value = api(&format!(
        "repos/artificial-intelligence-edge/PineWASM/git/commits/{revision}"
    ))?;
    let tree = value["tree"]["sha"]
        .as_str()
        .ok_or("missing immutable tree")?
        .to_owned();
    let parents = value["parents"]
        .as_array()
        .ok_or("missing commit parents")?
        .iter()
        .map(|p| {
            p["sha"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "missing parent SHA".to_owned())
        })
        .collect::<TaskResult<Vec<_>>>()?;
    Ok((tree, parents))
}

fn integrated_snapshot(
    plan: &Plan,
    run: &Value,
    tree: &str,
    parents: &[String],
    ancestors: &[(String, String)],
) -> Option<usize> {
    if !eligible_run(run) || plan.schema != SCHEMA || plan.tree != tree {
        return None;
    }
    let head = run["head_sha"].as_str()?;
    let identity_matches = plan.candidate == head
        || (run["event"] == "pull_request" && parents.iter().any(|p| p == head));
    if !identity_matches {
        return None;
    }
    // Push/dispatch runs still require their exact head commit in candidate
    // history. A squash commit preserves the PR's tested tree but not its head
    // SHA, so a successful same-repository PR run may instead match that tree
    // to an ancestor after its plan has been tied to the PR head above.
    let head_is_ancestor = ancestors.iter().any(|(sha, _)| sha == head);
    if !head_is_ancestor && run["event"] != "pull_request" {
        return None;
    }
    ancestors
        .iter()
        .position(|(_, candidate_tree)| candidate_tree == tree)
}

fn tree_bytes(root: &Path, revision: &str) -> TaskResult<Vec<u8>> {
    if let Ok(bytes) = git_bytes(root, &["ls-tree", "-rz", revision]) {
        return Ok(bytes);
    }
    if revision.len() != 40 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid source revision".into());
    }
    let tree = api(&format!(
        "repos/artificial-intelligence-edge/PineWASM/git/trees/{revision}?recursive=1"
    ))?;
    if tree["truncated"] != false {
        return Err("truncated producer tree".into());
    }
    let mut result = Vec::new();
    for item in tree["tree"].as_array().ok_or("missing producer tree")? {
        if item["type"] == "tree" {
            continue;
        }
        let field = |key| {
            item[key]
                .as_str()
                .ok_or_else(|| format!("missing tree {key}"))
        };
        let line = format!(
            "{} {} {}\t{}\0",
            field("mode")?,
            field("type")?,
            field("sha")?,
            field("path")?
        );
        result.extend_from_slice(line.as_bytes());
    }
    Ok(result)
}

fn workflow_source(root: &Path, revision: &str) -> TaskResult<String> {
    if let Ok(source) = git(
        root,
        &["show", &format!("{revision}:.github/workflows/ci.yml")],
    ) {
        return Ok(source);
    }
    String::from_utf8(request(
        &format!("repos/artificial-intelligence-edge/PineWASM/contents/.github/workflows/ci.yml?ref={revision}"),
        "application/vnd.github.raw+json", "1000000",
    )?).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn integrated_pr_requires_plan_identity_and_exact_tree() {
        let mut plan = Plan::empty(&[]);
        plan.candidate = "b".repeat(40);
        plan.tree = "c".repeat(40);
        let run = json!({"status":"completed","conclusion":"success","path":".github/workflows/ci.yml","head_repository":{"full_name":"artificial-intelligence-edge/PineWASM"},"event":"pull_request","head_sha":"a".repeat(40)});
        let ancestors = vec![
            ("d".repeat(40), plan.tree.clone()),
            ("a".repeat(40), "e".repeat(40)),
        ];
        let parents = vec!["a".repeat(40)];
        assert_eq!(
            integrated_snapshot(&plan, &run, &plan.tree, &parents, &ancestors),
            Some(0)
        );
        assert_eq!(
            integrated_snapshot(&plan, &run, &"f".repeat(40), &parents, &ancestors),
            None
        );
        assert_eq!(
            integrated_snapshot(&plan, &run, &plan.tree, &[], &ancestors),
            None
        );
        assert_eq!(
            integrated_snapshot(&plan, &run, &plan.tree, &parents, &ancestors[..1]),
            Some(0)
        );
        assert_eq!(
            integrated_snapshot(&plan, &run, &plan.tree, &parents, &ancestors[1..]),
            None
        );
        let mut cancelled = run.clone();
        cancelled["conclusion"] = "cancelled".into();
        assert_eq!(
            integrated_snapshot(&plan, &cancelled, &plan.tree, &parents, &ancestors),
            None
        );
        let mut push = run;
        push["event"] = "push".into();
        assert_eq!(
            integrated_snapshot(&plan, &push, &plan.tree, &parents, &ancestors),
            None
        );
    }

    #[test]
    fn squash_merged_pr_reuses_exact_tree_without_pr_head_ancestry() {
        // Fixed identities for a test in which a squash commit carries the exact
        // PR-head tree. The PR head SHA is not an ancestor.
        let pr_head = "1111111111111111111111111111111111111111";
        let pr_tree = "2222222222222222222222222222222222222222";
        let pr_base = "3333333333333333333333333333333333333333";
        let squash_commit = "4444444444444444444444444444444444444444";
        let current_base = "5555555555555555555555555555555555555555";
        let current_base_tree = "6666666666666666666666666666666666666666";

        let mut plan = Plan::empty(&[]);
        // Models the PR test merge candidate; its exact SHA is intentionally
        // irrelevant to this pure predicate, but its parent binds the run head.
        plan.candidate = "b".repeat(40);
        plan.tree = pr_tree.to_owned();
        let run = json!({
            "id":99,
            "run_attempt":2,
            "status":"completed",
            "conclusion":"success",
            "path":".github/workflows/ci.yml",
            "head_repository":{"full_name":"artificial-intelligence-edge/PineWASM"},
            "event":"pull_request",
            "head_sha":pr_head
        });
        let plan_parents = vec![pr_base.to_owned(), pr_head.to_owned()];
        let ancestors = vec![
            (current_base.to_owned(), current_base_tree.to_owned()),
            (squash_commit.to_owned(), pr_tree.to_owned()),
            (
                pr_base.to_owned(),
                "7777777777777777777777777777777777777777".to_owned(),
            ),
        ];
        assert!(!ancestors.iter().any(|(sha, _)| sha == pr_head));
        assert!(run_head_passes_preliminary_gate(&run, &ancestors));
        assert_eq!(
            integrated_snapshot(&plan, &run, pr_tree, &plan_parents, &ancestors),
            Some(1)
        );

        // The tree match is necessary; PR identity alone cannot seed reuse.
        assert_eq!(
            integrated_snapshot(&plan, &run, &"e".repeat(40), &plan_parents, &ancestors),
            None
        );
        let without_tree = vec![(current_base.to_owned(), current_base_tree.to_owned())];
        assert_eq!(
            integrated_snapshot(&plan, &run, pr_tree, &plan_parents, &without_tree),
            None
        );

        // Repo, run-success and logged-plan source checks remain mandatory.
        let mut wrong_repo = run.clone();
        wrong_repo["head_repository"]["full_name"] = "outsider/PineWASM".into();
        assert_eq!(
            integrated_snapshot(&plan, &wrong_repo, pr_tree, &plan_parents, &ancestors),
            None
        );
        let mut failed = run.clone();
        failed["conclusion"] = "cancelled".into();
        assert_eq!(
            integrated_snapshot(&plan, &failed, pr_tree, &plan_parents, &ancestors),
            None
        );
        let mut non_pr_run = run.clone();
        non_pr_run["event"] = "push".into();
        assert!(!run_head_passes_preliminary_gate(&non_pr_run, &ancestors));
        let head_plan = Plan {
            candidate: pr_head.to_owned(),
            ..plan.clone()
        };
        assert_eq!(
            integrated_snapshot(&head_plan, &non_pr_run, pr_tree, &plan_parents, &ancestors),
            None
        );
        let stale_run = json!({"head_sha":pr_head,"run_attempt":2});
        let stale_receipt = Receipt {
            schema: SCHEMA,
            lane: "msrv".into(),
            fingerprint: "fingerprint".into(),
            run_id: "99".into(),
            run_attempt: "1".into(),
            head_sha: pr_head.to_owned(),
            tested_commit: pr_head.to_owned(),
            repository: "artificial-intelligence-edge/PineWASM".into(),
        };
        assert!(!valid_receipt(
            &stale_receipt,
            &stale_run,
            "msrv",
            "fingerprint",
            "artificial-intelligence-edge/PineWASM",
            99
        ));
        let unrelated_plan = Plan {
            candidate: "c".repeat(40),
            ..plan.clone()
        };
        assert_eq!(
            integrated_snapshot(&unrelated_plan, &run, pr_tree, &[], &ancestors),
            None
        );
        let mut stale_plan = plan.clone();
        stale_plan.tree = "f".repeat(40);
        assert_eq!(
            integrated_snapshot(&stale_plan, &run, pr_tree, &plan_parents, &ancestors),
            None
        );
    }

    #[test]
    fn only_successful_final_gates_supply_well_formed_plans() {
        let mut plan = Plan::empty(&[]);
        plan.candidate = "a".repeat(40);
        plan.tree = "b".repeat(40);
        let encoded = serde_json::to_string(&plan).unwrap();
        assert_eq!(
            validated_plan_from_log(&format!("timestamp   CI_PLAN: {encoded}\n")),
            Some(plan.clone())
        );
        assert_eq!(
            validated_plan_from_log(&format!("timestamp PINEWASM_CI_VALIDATED_PLAN {encoded}\n")),
            Some(plan)
        );
        assert!(validated_plan_from_log("CI_PLAN: {}").is_none());
        assert!(validated_plan_from_log("no plan").is_none());
        for result in ["failure", "cancelled", "skipped", ""] {
            assert!(!successful_result_job(
                &json!({"name":"CI result","status":"completed","conclusion":result})
            ));
        }
        assert!(successful_result_job(
            &json!({"name":"CI result","status":"completed","conclusion":"success"})
        ));
    }

    #[test]
    fn direct_lookup_asks_for_base_and_parents_once_each() {
        let (a, b, c, d) = (
            "a".repeat(40),
            "b".repeat(40),
            "c".repeat(40),
            "d".repeat(40),
        );
        // A push: `before` is the first parent, the PR head the second, then
        // the merged PR head of `before`.
        assert_eq!(
            direct_candidates([a.clone(), a.clone(), b.clone(), c.clone()]),
            [a.clone(), b.clone(), c.clone()]
        );
        // Placeholders and malformed values are never queried.
        assert_eq!(
            direct_candidates(["none".into(), "0".repeat(39), "g".repeat(40), c.clone()]),
            vec![c.clone()]
        );
        assert!(direct_candidates([String::new()]).is_empty());
        // Bounded: at most three extra list calls per plan.
        assert_eq!(
            direct_candidates([a.clone(), b.clone(), c.clone(), d]),
            [a, b, c]
        );
    }

    #[test]
    fn direct_runs_are_filtered_here_and_rejections_are_named() {
        let page = json!({"workflow_runs":[
            {"id":1,"status":"completed","conclusion":"success"},
            {"id":2,"status":"completed","conclusion":"cancelled"},
            {"id":3,"status":"in_progress","conclusion":null},
            {"id":4,"status":"completed","conclusion":"success"}
        ]});
        let ids = successful_runs(&page)
            .iter()
            .map(|run| run["id"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, [1, 4]);
        assert!(successful_runs(&json!({"message":"Bad credentials"})).is_empty());
        let reasons = [
            Rejection::Malformed,
            Rejection::NotEligible,
            Rejection::HeadNotAncestor,
            Rejection::JobsLookupFailed,
            Rejection::NoResultJob,
            Rejection::LogFetchFailed,
            Rejection::NoValidatedPlan,
            Rejection::WorkflowLacksVerify,
            Rejection::CommitIdentityFailed,
            Rejection::TreeNotInAncestry,
        ]
        .map(Rejection::reason);
        assert_eq!(reasons.iter().collect::<BTreeSet<_>>().len(), reasons.len());
        assert_eq!(Rejection::HeadNotAncestor.reason(), "head not an ancestor");
    }

    #[test]
    fn a_run_seen_by_both_queries_is_checked_once() {
        let ancestors = vec![("a".repeat(40), "b".repeat(40))];
        let mut search = BaselineSearch {
            root: Path::new("."),
            repository: "artificial-intelligence-edge/PineWASM",
            ancestors: &ancestors,
            seen: BTreeSet::new(),
            best: None,
        };
        // Rejected before any network call: not eligible, then not an ancestor.
        let cancelled = json!({"id":7,"status":"completed","conclusion":"cancelled"});
        search.consider(&cancelled);
        search.consider(&cancelled);
        let unrelated = json!({"id":8,"status":"completed","conclusion":"success","path":".github/workflows/ci.yml","head_repository":{"full_name":"artificial-intelligence-edge/PineWASM"},"event":"push","head_sha":"f".repeat(40)});
        assert_eq!(search.check(&unrelated, 8), Err(Rejection::HeadNotAncestor));
        assert_eq!(search.check(&cancelled, 7), Err(Rejection::NotEligible));
        search.consider(&unrelated);
        assert_eq!(search.seen, BTreeSet::from([7, 8]));
        assert_eq!(search.baseline(), None);
    }

    #[test]
    fn log_transport_rejects_malformed_or_oversized_records() {
        assert!(receipt_from_log("no record").is_none());
        assert!(receipt_from_log("PINEWASM_CI_RECEIPT {bad}").is_none());
        assert!(receipt_from_log(&format!("PINEWASM_CI_RECEIPT {}", "x".repeat(16385))).is_none());
        let record = json!({"schema":1,"lane":"msrv","fingerprint":"key","run_id":"12","run_attempt":"1","head_sha":"a".repeat(40),"tested_commit":"b".repeat(40),"repository":"repo"});
        let parsed = receipt_from_log(&format!("PINEWASM_CI_RECEIPT {record}")).unwrap();
        assert_eq!(parsed.run_id, "12");
    }

    #[test]
    fn only_successful_same_repository_workflow_results_are_eligible() {
        let good = json!({"status":"completed","conclusion":"success","path":".github/workflows/ci.yml","head_repository":{"full_name":"artificial-intelligence-edge/PineWASM"},"event":"pull_request"});
        assert!(eligible_run(&good));
        for state in ["cancelled", "failure", "skipped", "timed_out", "neutral"] {
            let mut bad = good.clone();
            bad["conclusion"] = state.into();
            assert!(!eligible_run(&bad));
        }
        let mut bad = good.clone();
        bad["path"] = "other.yml".into();
        assert!(!eligible_run(&bad));
        let mut bad = good;
        bad["head_repository"]["full_name"] = "someone/fork".into();
        assert!(!eligible_run(&bad));
    }
    #[test]
    fn wrong_fingerprint_attempt_and_identity_are_rejected() {
        let run = json!({"head_sha":"a".repeat(40),"run_attempt":2});
        let mut receipt = Receipt {
            schema: SCHEMA,
            lane: "msrv".into(),
            fingerprint: "key".into(),
            run_id: "12".into(),
            run_attempt: "2".into(),
            head_sha: "a".repeat(40),
            tested_commit: "b".repeat(40),
            repository: "repo".into(),
        };
        assert!(valid_receipt(&receipt, &run, "msrv", "key", "repo", 12));
        assert!(!valid_receipt(&receipt, &run, "msrv", "other", "repo", 12));
        receipt.run_attempt = "1".into();
        assert!(!valid_receipt(&receipt, &run, "msrv", "key", "repo", 12));
    }
}
