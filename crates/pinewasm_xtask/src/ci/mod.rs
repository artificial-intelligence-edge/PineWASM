//! Dependency-aware CI. Selection is conservative; a cache is never a test result.
mod graph;
mod range;
mod receipt;
mod runner;
#[cfg(test)]
mod tests;

use crate::command::{git, git_bytes, pinned_cargo, repo_root};
use crate::TaskResult;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;

pub(super) const SCHEMA: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Plan {
    schema: u32,
    base: String,
    candidate: String,
    tree: String,
    full: bool,
    changes: Vec<String>,
    reasons: Vec<String>,
    // Empty target list means the complete package, otherwise exact integration targets.
    packages: BTreeMap<String, BTreeSet<String>>,
    docs: bool,
    runtime: bool,
    runtime_variants: BTreeSet<String>,
    safety: bool,
    wasm: bool,
    adversarial: bool,
    msrv: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Inventory {
    schema: u32,
    prose: BTreeSet<String>,
    runtime_packages: BTreeSet<String>,
    safety_test_prefixes: Vec<String>,
    fixture_prefixes: Vec<String>,
    fixture_consumers: BTreeSet<String>,
    #[serde(default)]
    fixture_groups: BTreeMap<String, BTreeSet<String>>,
}

pub(crate) fn plan(args: &[String]) -> TaskResult {
    let root = repo_root()?;
    let mut base = option(args, "--base")?
        .or_else(|| env::var("CI_BASE_SHA").ok())
        .ok_or("ci-plan requires --base or CI_BASE_SHA")?;
    let candidate = option(args, "--candidate")?.unwrap_or_else(|| "HEAD".into());
    let output = option(args, "--output")?;
    let github_output = option(args, "--github-output")?;
    validate_options(
        args,
        &["--base", "--candidate", "--output", "--github-output"],
    )?;
    let graph = graph::Graph::read(&root)?;
    let inventory: Inventory = toml::from_str(
        &fs::read_to_string(root.join(".github/ci/components.toml")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("invalid CI inventory: {e}"))?;
    if inventory.schema != SCHEMA {
        return Err("unsupported CI inventory schema".into());
    }
    for package in inventory
        .runtime_packages
        .union(&inventory.fixture_consumers)
    {
        if !graph.packages.contains_key(package) {
            return Err(format!("unknown package in CI inventory: {package}"));
        }
    }
    for (prefix, consumers) in &inventory.fixture_groups {
        if consumers.is_empty()
            || !prefix.ends_with('/')
            || !["docs/", "tests/", "benchmarks/"]
                .iter()
                .any(|root| prefix.starts_with(root))
        {
            return Err(format!("invalid fixture group {prefix}"));
        }
        for consumer in consumers {
            if !graph.packages.contains_key(consumer) {
                return Err(format!("unknown fixture consumer {consumer}"));
            }
        }
    }
    let head = git(
        &root,
        &["rev-parse", "--verify", &format!("{candidate}^{{commit}}")],
    )?
    .trim()
    .to_owned();
    if git(&root, &["rev-parse", "HEAD"])?.trim() != head {
        return Err("ci-plan must execute in the candidate checkout".into());
    }
    let tree = git(&root, &["rev-parse", &format!("{head}^{{tree}}")])?
        .trim()
        .to_owned();
    if env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
        base = match receipt::validated_baseline(&root, &head) {
            Ok(Some(base)) => base,
            Ok(None) => "no-validated-baseline".into(),
            Err(error) => {
                eprintln!("Baseline unavailable; full validation: {error}");
                "no-validated-baseline".into()
            }
        };
    }
    let resolved_base = git(
        &root,
        &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    );
    let (base, changes, fallback) = if let Ok(base) = resolved_base {
        let base = base.trim().to_owned();
        let changes = git_bytes(
            &root,
            &[
                "diff",
                "--name-only",
                "--no-renames",
                "-z",
                &base,
                &head,
                "--",
            ],
        )?;
        (base, paths(&changes)?, false)
    } else {
        (base, Vec::new(), true)
    };
    let mut result = select(&root, &graph, &inventory, &changes)?;
    result.base = base;
    result.candidate = head;
    result.tree = tree;
    if fallback || env::var("CI_FORCE_FULL").as_deref() == Ok("true") {
        result.make_full(&graph, "missing baseline or explicit full validation");
    }
    let encoded = serde_json::to_string(&result).map_err(|e| e.to_string())?;
    if let Some(path) = output {
        fs::write(path, format!("{encoded}\n")).map_err(|e| e.to_string())?;
    }
    if let Some(path) = github_output {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        for (key, value) in [
            ("plan", encoded.clone()),
            ("rust", (!result.packages.is_empty()).to_string()),
            ("docs", result.docs.to_string()),
            ("runtime", result.runtime.to_string()),
            ("safety", result.safety.to_string()),
            ("wasm", result.wasm.to_string()),
            ("adversarial", result.adversarial.to_string()),
            ("msrv", result.msrv.to_string()),
            ("full", result.full.to_string()),
        ] {
            writeln!(file, "{key}={value}").map_err(|e| e.to_string())?;
        }
    }
    println!("{encoded}");
    if let Ok(path) = env::var("GITHUB_STEP_SUMMARY") {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        writeln!(
            file,
            "### Validation selection\n\nCandidate: `{}`\n\nFull fallback: `{}`\n",
            result.candidate, result.full
        )
        .map_err(|e| e.to_string())?;
        for reason in &result.reasons {
            writeln!(file, "- {reason}").map_err(|e| e.to_string())?;
        }
        writeln!(file, "\n```json\n{encoded}\n```").map_err(|e| e.to_string())?;
    }
    Ok(())
}

impl Plan {
    fn empty(changes: &[String]) -> Self {
        Self {
            schema: SCHEMA,
            base: String::new(),
            candidate: String::new(),
            tree: String::new(),
            full: false,
            changes: changes.to_vec(),
            reasons: Vec::new(),
            packages: BTreeMap::new(),
            docs: false,
            runtime: false,
            runtime_variants: BTreeSet::new(),
            safety: false,
            wasm: false,
            adversarial: false,
            msrv: false,
        }
    }

    fn make_full(&mut self, graph: &graph::Graph, reason: &str) {
        self.full = true;
        self.packages = graph
            .packages
            .keys()
            .map(|name| (name.clone(), BTreeSet::new()))
            .collect();
        self.docs = true;
        self.runtime = true;
        self.runtime_variants = all_runtime_variants();
        self.safety = true;
        self.wasm = true;
        self.adversarial = true;
        self.msrv = true;
        self.reasons.push(reason.into());
    }
}

fn select(
    root: &Path,
    graph: &graph::Graph,
    inventory: &Inventory,
    changes: &[String],
) -> TaskResult<Plan> {
    let mut result = Plan::empty(changes);
    let mut changed_packages = BTreeSet::new();
    for path in changes {
        result.safety |= inventory
            .safety_test_prefixes
            .iter()
            .any(|prefix| path.starts_with(prefix));
        if inventory.prose.contains(path) {
            result
                .reasons
                .push(format!("{path}: prose and policy checks only"));
        } else if path.starts_with("crates/pinewasm_aot/runtime/") {
            changed_packages.insert("pinewasm_aot".to_owned());
            result.runtime = true;
        } else if inventory
            .fixture_groups
            .keys()
            .any(|prefix| path.starts_with(prefix))
        {
            for (prefix, consumers) in &inventory.fixture_groups {
                if path.starts_with(prefix) {
                    for consumer in consumers {
                        result.packages.insert(consumer.clone(), BTreeSet::new());
                    }
                }
            }
            result
                .reasons
                .push(format!("{path}: declared fixture consumers"));
        } else if inventory
            .fixture_prefixes
            .iter()
            .any(|prefix| path.starts_with(prefix))
        {
            for consumer in &inventory.fixture_consumers {
                result.packages.insert(consumer.clone(), BTreeSet::new());
            }
            result.adversarial = true;
            result
                .reasons
                .push(format!("{path}: fixture consumers and correctness tests"));
        } else if let Some((name, package)) = graph.owner(path) {
            if name == "pinewasm_xtask" || path.ends_with("Cargo.toml") {
                result.make_full(
                    graph,
                    &format!("{path}: automation or dependency graph changed"),
                );
                break;
            }
            if let Some(target) = package
                .tests
                .iter()
                .find_map(|(target, source)| (source == path).then_some(target))
            {
                if graph.is_isolated_test(root, path)? {
                    result.adversarial |=
                        crate::repository::owns_adversarial_target(name, Some(target));
                    match result.packages.entry(name.clone()) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            entry.insert(BTreeSet::from([target.clone()]));
                        }
                        std::collections::btree_map::Entry::Occupied(mut entry)
                            if !entry.get().is_empty() =>
                        {
                            entry.get_mut().insert(target.clone());
                        }
                        std::collections::btree_map::Entry::Occupied(_) => {}
                    }
                    result.reasons.push(format!(
                        "{path}: isolated integration target {name}/{target}"
                    ));
                    continue;
                }
            }
            if path.starts_with(&format!("{}/tests/", package.directory)) {
                // Shared/deleted test inputs invalidate this package's test suite,
                // not unrelated product consumers.
                result.adversarial |= crate::repository::owns_adversarial_target(name, None);
                result.packages.insert(name.clone(), BTreeSet::new());
                result
                    .reasons
                    .push(format!("{path}: complete owning test suite"));
            } else {
                changed_packages.insert(name.clone());
                result
                    .reasons
                    .push(format!("{path}: implementation/build input and consumers"));
            }
        } else {
            result.make_full(
                graph,
                &format!("{path}: cross-cutting or unclassified input"),
            );
            break;
        }
    }
    if !result.full {
        for package in graph.consumers(&changed_packages) {
            result.packages.insert(package, BTreeSet::new());
        }
        result.docs = !changed_packages.is_empty();
        result.adversarial |= !changed_packages.is_empty();
        result.wasm =
            result.packages.contains_key("pinewasm_bindings") && !changed_packages.is_empty();
        result.runtime |= !changed_packages.is_disjoint(&inventory.runtime_packages);
        if result.runtime {
            result.runtime_variants = selected_runtime_variants(changes);
        }
        result.safety |= result.runtime;
        result.msrv = !result.packages.is_empty();
    }
    Ok(result)
}

fn all_runtime_variants() -> BTreeSet<String> {
    ["base", "ta", "standalone_wasm", "broker"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

fn selected_runtime_variants(changes: &[String]) -> BTreeSet<String> {
    let mut variants = BTreeSet::new();
    for path in changes {
        let selected: &[&str] = if path.starts_with("crates/pinewasm_broker/")
            || path == "crates/pinewasm_aot/runtime/src/broker.rs"
            || path == "crates/pinewasm_aot/runtime/pinewasm_aot_runtime_broker.wasm"
        {
            &["broker"]
        } else if path.starts_with("crates/pinewasm_ta/")
            || path == "crates/pinewasm_aot/runtime/src/ta.rs"
            || path == "crates/pinewasm_aot/runtime/pinewasm_aot_runtime_ta.wasm"
        {
            &["ta", "standalone_wasm", "broker"]
        } else if path == "crates/pinewasm_aot/runtime/src/standalone_wasm.rs"
            || path == "crates/pinewasm_aot/runtime/pinewasm_aot_runtime_standalone_wasm.wasm"
        {
            &["standalone_wasm", "broker"]
        } else if path.starts_with("crates/pinewasm_num/")
            || path.starts_with("crates/pinewasm_aot/runtime/")
        {
            return all_runtime_variants();
        } else {
            &[]
        };
        variants.extend(selected.iter().map(|name| (*name).to_owned()));
    }
    if variants.is_empty() {
        all_runtime_variants()
    } else {
        variants
    }
}

pub(super) fn read_plan(root: &Path) -> TaskResult<Plan> {
    let encoded =
        env::var("CI_PLAN").map_err(|_| "CI_PLAN must contain the planner's JSON".to_owned())?;
    let plan: Plan = serde_json::from_str(&encoded).map_err(|e| format!("invalid CI_PLAN: {e}"))?;
    if plan.schema != SCHEMA
        || plan.candidate != git(root, &["rev-parse", "HEAD"])?.trim()
        || plan.tree != git(root, &["rev-parse", "HEAD^{tree}"])?.trim()
    {
        return Err("plan schema or candidate identity does not match this checkout".into());
    }
    Ok(plan)
}

pub(super) fn paths(bytes: &[u8]) -> TaskResult<Vec<String>> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| {
            let path = std::str::from_utf8(part).map_err(|e| e.to_string())?;
            if path.contains(['\n', '\r'])
                || path.starts_with('/')
                || path.split('/').any(|p| p == "..")
            {
                return Err("unsupported repository path in CI input".into());
            }
            Ok(path.to_owned())
        })
        .collect()
}

pub(super) fn option(args: &[String], name: &str) -> TaskResult<Option<String>> {
    let positions = args
        .iter()
        .enumerate()
        .filter_map(|(i, arg)| (arg == name).then_some(i))
        .collect::<Vec<_>>();
    match positions.as_slice() {
        [] => Ok(None),
        [i] => args
            .get(i + 1)
            .filter(|value| !value.starts_with("--"))
            .cloned()
            .map(Some)
            .ok_or_else(|| format!("missing value for {name}")),
        _ => Err(format!("duplicate {name}")),
    }
}

fn validate_options(args: &[String], names: &[&str]) -> TaskResult {
    if args.len() % 2 != 0
        || args
            .chunks(2)
            .any(|pair| !names.contains(&pair[0].as_str()))
    {
        return Err("unexpected CI command arguments".into());
    }
    Ok(())
}

pub(crate) use range::range_base;
pub(crate) use receipt::{record, reuse};
pub(crate) use runner::{execute, verify};
