use super::*;
use crate::command::{run, temp_dir};
use graph::{Graph, Package};
use std::process::Command;

fn graph() -> Graph {
    let mut packages = BTreeMap::new();
    for (name, deps) in [
        ("pinewasm_num", vec![]),
        ("pinewasm_aot", vec!["pinewasm_num"]),
        ("pinewasm_bindings", vec!["pinewasm_aot"]),
        ("pinewasm_xtask", vec![]),
    ] {
        packages.insert(
            name.into(),
            Package {
                directory: format!("crates/{name}"),
                dependencies: deps.into_iter().map(str::to_owned).collect(),
                tests: if name == "pinewasm_aot" {
                    BTreeMap::from([(
                        "isolated".into(),
                        "crates/pinewasm_aot/tests/isolated.rs".into(),
                    )])
                } else {
                    BTreeMap::new()
                },
                features: BTreeSet::new(),
            },
        );
    }
    Graph { packages }
}

fn inventory() -> Inventory {
    Inventory {
        schema: SCHEMA,
        prose: BTreeSet::from(["docs/architecture.md".into()]),
        safety_test_prefixes: vec!["crates/pinewasm_num/tests/".into()],
        runtime_packages: BTreeSet::from(["pinewasm_num".into()]),
        fixture_prefixes: vec!["tests/regression/".into()],
        fixture_consumers: BTreeSet::from(["pinewasm_aot".into()]),
        fixture_groups: BTreeMap::new(),
    }
}

#[test]
fn prose_never_builds_products_or_runs_unrelated_tests() {
    let plan = select(
        Path::new("."),
        &graph(),
        &inventory(),
        &["docs/architecture.md".into()],
    )
    .unwrap();
    assert!(plan.packages.is_empty());
    assert!(!plan.wasm && !plan.runtime && !plan.docs && !plan.adversarial);
}

#[test]
fn numeric_source_reaches_runtime_and_reverse_consumers() {
    let plan = select(
        Path::new("."),
        &graph(),
        &inventory(),
        &["crates/pinewasm_num/src/lib.rs".into()],
    )
    .unwrap();
    assert_eq!(plan.packages.len(), 3);
    assert!(plan.runtime && plan.wasm && plan.docs && plan.msrv);
    assert!(!plan.packages.contains_key("pinewasm_xtask"));
}

#[test]
fn runtime_blob_is_a_build_input_outside_cargo_workspace() {
    let plan = select(
        Path::new("."),
        &graph(),
        &inventory(),
        &["crates/pinewasm_aot/runtime/pinewasm_aot_runtime.wasm".into()],
    )
    .unwrap();
    assert!(plan.runtime && plan.wasm);
    assert!(plan.packages.contains_key("pinewasm_aot"));
}

#[test]
fn configuration_unknown_and_selector_edits_fall_back_to_full() {
    for path in [
        "Cargo.lock",
        "new-input.bin",
        ".github/ci/components.toml",
        "crates/pinewasm_aot/Cargo.toml",
        "crates/pinewasm_xtask/src/ci/mod.rs",
    ] {
        let plan = select(Path::new("."), &graph(), &inventory(), &[path.into()]).unwrap();
        assert!(plan.full && plan.wasm && plan.runtime);
        assert_eq!(plan.packages.len(), 4);
    }
}

#[test]
fn deleted_test_selects_own_suite_without_product_build() {
    let plan = select(
        Path::new("."),
        &graph(),
        &inventory(),
        &["crates/pinewasm_aot/tests/removed.rs".into()],
    )
    .unwrap();
    assert_eq!(plan.packages.len(), 1);
    assert!(plan.packages["pinewasm_aot"].is_empty());
    assert!(!plan.runtime && !plan.wasm && !plan.docs);
}

#[test]
fn isolated_regression_and_shared_helper_are_order_independent() {
    let temp = temp_dir("ci-selector-tests").unwrap();
    run(Command::new("git").arg("init").arg(temp.path())).unwrap();
    let test = "crates/pinewasm_aot/tests/isolated.rs";
    fs::create_dir_all(temp.path().join("crates/pinewasm_aot/tests")).unwrap();
    fs::write(
        temp.path().join(test),
        "#[test]\nfn regression() { assert_eq!(1, 1); }\n",
    )
    .unwrap();
    run(Command::new("git")
        .arg("-C")
        .arg(temp.path())
        .args(["add", "."]))
    .unwrap();
    let plan = select(temp.path(), &graph(), &inventory(), &[test.into()]).unwrap();
    assert_eq!(
        plan.packages["pinewasm_aot"],
        BTreeSet::from(["isolated".into()])
    );
    assert!(!plan.wasm && !plan.runtime && !plan.docs && !plan.adversarial);
    for changes in [
        vec![test.into(), "crates/pinewasm_aot/tests/helper.rs".into()],
        vec!["crates/pinewasm_aot/tests/helper.rs".into(), test.into()],
    ] {
        let plan = select(temp.path(), &graph(), &inventory(), &changes).unwrap();
        assert!(plan.packages["pinewasm_aot"].is_empty());
    }
    fs::write(
        temp.path().join(test),
        "mod support;\n#[test] fn regression() {}\n",
    )
    .unwrap();
    let plan = select(temp.path(), &graph(), &inventory(), &[test.into()]).unwrap();
    assert!(plan.packages["pinewasm_aot"].is_empty());
}

#[test]
fn development_dependency_cycles_terminate() {
    let mut graph = graph();
    graph
        .packages
        .get_mut("pinewasm_num")
        .unwrap()
        .dependencies
        .insert("pinewasm_aot".into());
    assert_eq!(
        graph
            .dependencies(&BTreeSet::from(["pinewasm_aot".into()]))
            .len(),
        2
    );
    assert_eq!(
        graph
            .consumers(&BTreeSet::from(["pinewasm_num".into()]))
            .len(),
        3
    );
}

#[test]
fn nul_delimited_diff_preserves_spaces_and_rejects_unsafe_paths() {
    assert_eq!(paths(b"a b.rs\0c.rs\0").unwrap(), ["a b.rs", "c.rs"]);
    assert!(paths(b"../outside\0").is_err());
    assert!(paths(b"bad\npath\0").is_err());
}

#[test]
fn safety_harness_change_does_not_rebuild_runtime_product() {
    let plan = select(
        Path::new("."),
        &graph(),
        &inventory(),
        &["crates/pinewasm_num/tests/pinned_baseline.rs".into()],
    )
    .unwrap();
    assert!(plan.safety);
    assert!(!plan.runtime && !plan.wasm);
}

#[test]
fn runtime_variants_follow_feature_dependencies() {
    assert_eq!(
        selected_runtime_variants(&["crates/pinewasm_broker/src/lib.rs".into()]),
        BTreeSet::from(["broker".into()])
    );
    assert_eq!(
        selected_runtime_variants(&["crates/pinewasm_ta/src/engine.rs".into()]),
        BTreeSet::from(["ta".into(), "standalone_wasm".into(), "broker".into()])
    );
    assert_eq!(
        selected_runtime_variants(&["crates/pinewasm_aot/runtime/src/standalone_wasm.rs".into()]),
        BTreeSet::from(["standalone_wasm".into(), "broker".into()])
    );
    for path in [
        "crates/pinewasm_num/src/lib.rs",
        "crates/pinewasm_aot/runtime/src/lib.rs",
    ] {
        assert_eq!(
            selected_runtime_variants(&[path.into()]),
            all_runtime_variants()
        );
    }
}

#[test]
fn ignored_campaign_targets_keep_their_explicit_execution() {
    assert!(crate::repository::owns_adversarial_target(
        "pinewasm_aot",
        Some("differential")
    ));
    assert!(crate::repository::owns_adversarial_target(
        "pinewasm_aot",
        None
    ));
    assert!(!crate::repository::owns_adversarial_target(
        "pinewasm_aot",
        Some("isolated")
    ));
}

#[test]
fn shared_fixture_selects_consumers_without_building_a_new_product() {
    let plan = select(
        Path::new("."),
        &graph(),
        &inventory(),
        &["tests/regression/example.pine".into()],
    )
    .unwrap();
    assert!(plan.packages.contains_key("pinewasm_aot") && plan.adversarial);
    assert!(!plan.wasm && !plan.runtime && !plan.docs);
}

#[test]
fn grouped_fixtures_do_not_build_products_or_run_unrelated_campaigns() {
    let mut inventory = inventory();
    inventory.fixture_groups.insert(
        "docs/fixtures/".into(),
        BTreeSet::from(["pinewasm_aot".into()]),
    );
    let plan = select(
        Path::new("."),
        &graph(),
        &inventory,
        &["docs/fixtures/records.json".into()],
    )
    .unwrap();
    assert_eq!(
        plan.packages.keys().cloned().collect::<Vec<_>>(),
        ["pinewasm_aot"]
    );
    assert!(!plan.full && !plan.runtime && !plan.wasm && !plan.safety && !plan.adversarial);
    assert!(plan.msrv);
    let unknown = select(
        Path::new("."),
        &graph(),
        &inventory,
        &["docs/unknown/data.json".into()],
    )
    .unwrap();
    assert!(unknown.full);
}
