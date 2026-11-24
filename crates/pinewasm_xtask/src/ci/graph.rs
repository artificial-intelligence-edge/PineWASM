use super::{paths, pinned_cargo};
use crate::command::{git_bytes, output};
use crate::TaskResult;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

#[derive(Debug)]
pub(super) struct Package {
    pub directory: String,
    pub dependencies: BTreeSet<String>,
    pub tests: BTreeMap<String, String>,
    pub features: BTreeSet<String>,
}

#[derive(Debug)]
pub(super) struct Graph {
    pub packages: BTreeMap<String, Package>,
}

impl Graph {
    pub fn read(root: &Path) -> TaskResult<Self> {
        let root = root.canonicalize().map_err(|e| e.to_string())?;
        let metadata = output(pinned_cargo(&root)?.args([
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--all-features",
        ]))?;
        let metadata: Value =
            serde_json::from_slice(&metadata.stdout).map_err(|e| e.to_string())?;
        let members = metadata["workspace_members"]
            .as_array()
            .ok_or("missing workspace members")?;
        let mut packages = BTreeMap::new();
        for package in metadata["packages"].as_array().ok_or("missing packages")? {
            if !members.contains(&package["id"]) {
                continue;
            }
            let name = string(&package["name"])?;
            let manifest = Path::new(string(&package["manifest_path"])?)
                .canonicalize()
                .map_err(|e| e.to_string())?;
            let directory = manifest
                .parent()
                .ok_or("manifest has no parent")?
                .strip_prefix(&root)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            let mut tests = BTreeMap::new();
            for target in package["targets"].as_array().ok_or("missing targets")? {
                if target["kind"]
                    .as_array()
                    .is_some_and(|kinds| kinds.iter().any(|kind| kind == "test"))
                {
                    let source = Path::new(string(&target["src_path"])?)
                        .canonicalize()
                        .map_err(|e| e.to_string())?;
                    tests.insert(
                        string(&target["name"])?.to_owned(),
                        source
                            .strip_prefix(&root)
                            .map_err(|e| e.to_string())?
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
            let dependencies = package["dependencies"]
                .as_array()
                .ok_or("missing dependencies")?
                .iter()
                .filter_map(|dep| dep["path"].as_str().map(|_| dep["name"].as_str()))
                .flatten()
                .map(str::to_owned)
                .collect();
            let features = package["features"]
                .as_object()
                .ok_or("missing features")?
                .keys()
                .cloned()
                .collect();
            packages.insert(
                name.to_owned(),
                Package {
                    directory,
                    dependencies,
                    tests,
                    features,
                },
            );
        }
        if packages.is_empty() {
            return Err("empty workspace graph".into());
        }
        Ok(Self { packages })
    }

    pub fn owner(&self, path: &str) -> Option<(&String, &Package)> {
        self.packages
            .iter()
            .find(|(_, package)| path.starts_with(&format!("{}/", package.directory)))
    }

    pub fn consumers(&self, seeds: &BTreeSet<String>) -> BTreeSet<String> {
        let mut result = seeds.clone();
        loop {
            let previous = result.len();
            for (name, package) in &self.packages {
                if !package.dependencies.is_disjoint(&result) {
                    result.insert(name.clone());
                }
            }
            if result.len() == previous {
                return result;
            }
        }
    }

    pub fn dependencies(&self, seeds: &BTreeSet<String>) -> BTreeSet<String> {
        let mut result = seeds.clone();
        loop {
            let previous = result.len();
            for name in result.clone() {
                if let Some(package) = self.packages.get(&name) {
                    result.extend(package.dependencies.iter().cloned());
                }
            }
            if result.len() == previous {
                return result;
            }
        }
    }

    pub fn is_isolated_test(&self, root: &Path, path: &str) -> TaskResult<bool> {
        if !root.join(path).is_file() {
            return Ok(false);
        }
        let source = fs::read_to_string(root.join(path)).map_err(|e| e.to_string())?;
        // Includes, external modules and filesystem or process access need wider ownership.
        if [
            "include",
            "mod ",
            "#[path",
            "fs::",
            "File::",
            "Command::",
            "env::",
            "env!",
            "option_env!",
        ]
        .iter()
        .any(|token| source.contains(token))
        {
            return Ok(false);
        }
        let filename = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("invalid test filename")?;
        let stem = Path::new(path)
            .file_stem()
            .and_then(|name| name.to_str())
            .ok_or("invalid test stem")?;
        // A target can itself be imported by another target or a production include.
        for candidate in paths(&git_bytes(root, &["ls-files", "-z", "--", "*.rs"])?)? {
            if candidate != path && {
                let text = fs::read_to_string(root.join(&candidate)).map_err(|e| e.to_string())?;
                text.contains(filename) || text.contains(&format!("mod {stem}"))
            } {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn string(value: &Value) -> TaskResult<&str> {
    value
        .as_str()
        .ok_or_else(|| "invalid cargo metadata string".into())
}
