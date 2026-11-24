use super::{graph::Graph, option, read_plan, validate_options};
use crate::command::{output, pinned_cargo, repo_root, run};
use crate::{repository, TaskResult};
use serde_json::Value;
use std::collections::BTreeSet;
use std::env;
use std::path::Path;
use std::process::Command;

pub(crate) fn execute(args: &[String]) -> TaskResult {
    validate_options(args, &["--stage"])?;
    let stage = option(args, "--stage")?.ok_or("ci-run requires --stage")?;
    let root = repo_root()?;
    let plan = read_plan(&root)?;
    if plan.packages.is_empty() {
        return Err("no Rust tasks selected; do not invoke ci-run".into());
    }
    let graph = Graph::read(&root)?;
    match stage.as_str() {
        "runtime" if plan.runtime => crate::aot::rebuild_selected(&plan.runtime_variants),
        "default" | "all-features" | "lint" | "msrv" | "msrv-all-features" => {
            let whole = plan
                .packages
                .iter()
                .filter(|(_, targets)| targets.is_empty())
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>();
            if !whole.is_empty() {
                let mut command =
                    selected_command(&root, &graph, &stage, &whole, &BTreeSet::new())?;
                run(&mut command)?;
            }
            for (name, targets) in &plan.packages {
                if targets.is_empty() {
                    continue;
                }
                let package = graph
                    .packages
                    .get(name)
                    .ok_or_else(|| format!("unknown package {name}"))?;
                if targets.iter().any(|name| !package.tests.contains_key(name)) {
                    return Err(format!("unknown test target in package {name}"));
                }
                let mut command =
                    selected_command(&root, &graph, &stage, &[name.as_str()], targets)?;
                if !targets.is_empty() && matches!(stage.as_str(), "default" | "all-features") {
                    // Fail closed on a stale/empty target rather than claiming a green test.
                    for target in targets {
                        let mut discovery = selected_command(
                            &root,
                            &graph,
                            &stage,
                            &[name.as_str()],
                            &BTreeSet::from([target.clone()]),
                        )?;
                        let listing = output(discovery.args(["--", "--list"]))?;
                        if !String::from_utf8_lossy(&listing.stdout)
                            .lines()
                            .any(|line| line.ends_with(": test"))
                        {
                            return Err(format!("{name}/{target}: no tests discovered"));
                        }
                    }
                }
                run(&mut command)?;
            }
            if stage == "default"
                && plan
                    .packages
                    .get("pinewasm_compiler")
                    .is_some_and(BTreeSet::is_empty)
            {
                run(pinned_cargo(&root)?
                    .env("PINEWASM_VALIDATE_PASSES", "1")
                    .args([
                        "run",
                        "--locked",
                        "-p",
                        "pinewasm_compiler",
                        "--example",
                        "embedding",
                    ]))?;
            }
            if stage == "lint" && plan.packages.contains_key("pinewasm_bindings") {
                run(pinned_cargo(&root)?.args([
                    "clippy",
                    "--locked",
                    "-p",
                    "pinewasm_bindings",
                    "--lib",
                    "--target",
                    "wasm32-unknown-unknown",
                    "--",
                    "-D",
                    "warnings",
                ]))?;
            }
            Ok(())
        }
        "minimal" => {
            for (name, lib_only) in [
                ("pinewasm_broker", true),
                ("pinewasm_ta", false),
                ("pinewasm_lsp", true),
                ("pinewasm_num", false),
            ] {
                let Some(targets) = plan.packages.get(name) else {
                    continue;
                };
                if lib_only && !targets.is_empty() {
                    continue;
                }
                let mut command = pinned_cargo(&root)?;
                command.args(["test", "--locked", "-p", name, "--no-default-features"]);
                if lib_only {
                    command.arg("--lib");
                }
                for target in targets {
                    command.args(["--test", target]);
                }
                run(&mut command)?;
            }
            Ok(())
        }
        "feature-check" => {
            if plan.full {
                repository::feature_check(&[])
            } else {
                Ok(())
            }
        }
        _ => Err(format!("unknown CI stage: {stage}")),
    }
}

fn selected_command(
    root: &Path,
    graph: &Graph,
    stage: &str,
    names: &[&str],
    targets: &BTreeSet<String>,
) -> TaskResult<Command> {
    let mut command = if stage.starts_with("msrv") {
        // The floor toolchain is deliberately independent of the xtask compiler.
        let version = env::var("PINEWASM_RUST_MSRV").map_err(|_| "missing minimum toolchain")?;
        if !version.chars().all(|c| c.is_ascii_digit() || c == '.') {
            return Err("invalid minimum toolchain".into());
        }
        let mut command = Command::new("cargo");
        command.arg(format!("+{version}"));
        command
    } else {
        pinned_cargo(root)?
    };
    command.current_dir(root);
    command.env_remove("PINEWASM_VALIDATE_PASSES");
    command.arg(match stage {
        "lint" => "clippy",
        "msrv" | "msrv-all-features" => "check",
        _ => "test",
    });
    command.arg("--locked");
    let selected = if matches!(stage, "all-features" | "lint" | "msrv-all-features") {
        // Cargo only accepts qualified features for selected packages or their
        // direct dependencies. Select the closure explicitly to preserve the
        // old workspace-wide feature unification (including numeric miri).
        graph.dependencies(&names.iter().map(|name| (*name).to_owned()).collect())
    } else {
        names.iter().map(|name| (*name).to_owned()).collect()
    };
    for name in &selected {
        command.args(["-p", name]);
    }
    if targets.is_empty() && matches!(stage, "lint" | "msrv" | "msrv-all-features") {
        command.arg("--all-targets");
    }
    for target in targets {
        command.args(["--test", target]);
    }
    if matches!(stage, "all-features" | "lint" | "msrv-all-features") {
        let closure = graph.dependencies(&names.iter().map(|name| (*name).to_owned()).collect());
        let features = closure
            .iter()
            .filter_map(|name| graph.packages.get(name).map(|p| (name, p)))
            .flat_map(|(name, package)| {
                package
                    .features
                    .iter()
                    .map(move |feature| format!("{name}/{feature}"))
            })
            .collect::<Vec<_>>();
        command.arg("--all-features");
        if !features.is_empty() {
            command.args(["--features", &features.join(",")]);
        }
    }
    if stage == "default" {
        command.env("PINEWASM_VALIDATE_PASSES", "1");
    }
    if stage == "lint" {
        command.args(["--", "-D", "warnings"]);
    }
    Ok(command)
}

pub(crate) fn verify(args: &[String]) -> TaskResult {
    if !args.is_empty() {
        return Err("ci-verify accepts no arguments".into());
    }
    let plan = read_plan(&repo_root()?)?;
    let needs: Value = serde_json::from_str(&env::var("CI_NEEDS").map_err(|_| "missing CI_NEEDS")?)
        .map_err(|e| e.to_string())?;
    let required = [
        ("plan", true),
        ("guard", true),
        ("workflow-policy", true),
        ("secret-scan", true),
        ("dco", true),
        ("dependency-policy", true),
        ("rust", !plan.packages.is_empty()),
        ("adversarial", plan.adversarial),
        ("msrv", plan.msrv),
        ("aot-runtime-reproducibility", plan.runtime),
        ("runtime-safety", plan.safety),
        ("wasm", plan.wasm),
    ];
    verify_results(&needs, &required)?;
    println!(
        "PINEWASM_CI_VALIDATED_PLAN {}",
        serde_json::to_string(&plan).map_err(|e| e.to_string())?
    );
    println!("Every required task passed or reused verified success; unaffected jobs were explicitly omitted.");
    Ok(())
}

fn verify_results(needs: &Value, required: &[(&str, bool)]) -> TaskResult {
    for &(name, required) in required {
        let state = needs[name]["result"]
            .as_str()
            .ok_or_else(|| format!("missing result for {name}"))?;
        if state != "success" && (required || state != "skipped") {
            return Err(format!(
                "{name}: expected {}, got {state}",
                if required {
                    "success"
                } else {
                    "success or not affected"
                }
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::verify_results;
    use serde_json::json;
    #[test]
    fn skipped_required_failed_and_missing_results_never_pass() {
        for value in [
            json!({}),
            json!({"rust":{"result":"skipped"}}),
            json!({"rust":{"result":"failure"}}),
            json!({"rust":{"result":"cancelled"}}),
        ] {
            assert!(verify_results(&value, &[("rust", true)]).is_err());
        }
        assert!(verify_results(&json!({"rust":{"result":"success"}}), &[("rust", true)]).is_ok());
        assert!(verify_results(&json!({"rust":{"result":"skipped"}}), &[("rust", false)]).is_ok());
    }
}
