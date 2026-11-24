use crate::command::repo_root;
use crate::TaskResult;
use regex::Regex;
use std::env;
use std::fs;

pub(crate) fn check() -> TaskResult {
    self_test()?;
    let root = repo_root()?;
    let directory = root.join(".github/workflows");
    let mut workflows = fs::read_dir(&directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot enumerate {}: {error}", directory.display()))?
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| {
            matches!(
                path.extension().and_then(|value| value.to_str()),
                Some("yml" | "yaml")
            )
        })
        .collect::<Vec<_>>();
    workflows.sort();
    if workflows.is_empty() {
        return Err("no GitHub Actions workflows found".into());
    }
    let mut failures = Vec::new();
    for workflow in &workflows {
        let source = fs::read_to_string(workflow)
            .map_err(|error| format!("cannot read {}: {error}", workflow.display()))?;
        for (line_number, code) in workflow_code_lines(&source) {
            match line_reference(&code) {
                Err(reason) => failures.push(format!(
                    "{}:{line_number}: {reason}",
                    workflow.strip_prefix(&root).unwrap_or(workflow).display()
                )),
                Ok(Some(reference)) if !immutable(&reference) => failures.push(format!(
                    "{}:{line_number}: mutable uses reference {reference:?}",
                    workflow.strip_prefix(&root).unwrap_or(workflow).display()
                )),
                _ => {}
            }
        }
        if workflow.file_name().and_then(|name| name.to_str()) == Some("ci.yml") {
            if let Err(reason) = check_msrv_xtask_lane(&source) {
                failures.push(format!(
                    "{}: {reason}",
                    workflow.strip_prefix(&root).unwrap_or(workflow).display()
                ));
            }
        }
    }
    if !failures.is_empty() {
        for failure in &failures {
            if env::var_os("GITHUB_ACTIONS").is_some() {
                let file = failure.split_once(':').map_or("", |(file, _)| file);
                eprintln!("::error file={file}::{failure}");
            } else {
                eprintln!("ERROR: {failure}");
            }
        }
        return Err(format!("{} workflow policy violation(s)", failures.len()));
    }
    println!(
        "Workflow action pins are immutable and required test lanes are complete ({} workflow file(s)).",
        workflows.len()
    );
    Ok(())
}

fn check_msrv_xtask_lane(source: &str) -> Result<(), String> {
    let candidates = continued_shell_commands(source)
        .into_iter()
        .filter(|command| {
            command.contains("PINEWASM_FIXTURE_RUST_TOOLCHAIN=")
                && command.contains("pinewasm_xtask")
                && command.contains(" test ")
        })
        .collect::<Vec<_>>();
    let [command] = candidates.as_slice() else {
        return Err(format!(
            "expected exactly one MSRV xtask fixture command, found {}",
            candidates.len()
        ));
    };
    for required in [
        "PINEWASM_FIXTURE_RUST_TOOLCHAIN=\"${PINEWASM_RUST_MSRV}\"",
        "cargo \"+${PINEWASM_RUST_MSRV}\" test",
        "--locked -p pinewasm_xtask --all-targets",
    ] {
        if !command.contains(required) {
            return Err(format!(
                "MSRV xtask fixture command must contain {required:?}: {command}"
            ));
        }
    }
    for limiting_flag in [
        "--bin",
        "--bins",
        "--lib",
        "--test",
        "--tests",
        "--example",
        "--examples",
    ] {
        if command
            .split_ascii_whitespace()
            .any(|word| word == limiting_flag)
        {
            return Err(format!(
                "MSRV xtask fixture command must not use target-limiting flag {limiting_flag}: {command}"
            ));
        }
    }
    Ok(())
}

fn continued_shell_commands(source: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut current = String::new();
    for raw in source.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (part, continues) = line
            .strip_suffix('\\')
            .map_or((line, false), |part| (part.trim_end(), true));
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(part);
        if !continues {
            commands.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        commands.push(current);
    }
    commands
}

fn immutable(reference: &str) -> bool {
    let reference = reference.trim_matches(['\'', '"']);
    if reference.starts_with("./") {
        return true;
    }
    if let Some((name, digest)) = reference.rsplit_once('@') {
        if name.starts_with("docker://") {
            return digest.strip_prefix("sha256:").is_some_and(|hash| {
                hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
        }
        return !name.is_empty()
            && digest.len() == 40
            && digest.bytes().all(|byte| byte.is_ascii_hexdigit());
    }
    false
}

fn strip_yaml_comment(line: &str) -> &str {
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && double {
            escaped = true;
        } else if character == '\'' && !double {
            single = !single;
        } else if character == '"' && !single {
            double = !double;
        } else if character == '#' && !single && !double {
            return &line[..index];
        }
    }
    line
}

fn workflow_code_lines(text: &str) -> Vec<(usize, String)> {
    let scalar = Regex::new(r":[ \t]*[>|][+-]?[1-9]?[ \t]*$").expect("valid regex");
    let mut result = Vec::new();
    let mut block_parent_indent = None;
    for (index, raw) in text.lines().enumerate() {
        let stripped = raw.trim_start_matches(' ');
        let indent = raw.len() - stripped.len();
        if let Some(parent) = block_parent_indent {
            if stripped.is_empty() || indent > parent {
                continue;
            }
            block_parent_indent = None;
        }
        let code = strip_yaml_comment(raw).trim_end();
        if code.trim().is_empty() {
            continue;
        }
        result.push((index + 1, code.to_string()));
        if scalar.is_match(code) {
            block_parent_indent = Some(indent);
        }
    }
    result
}

fn line_reference(code: &str) -> Result<Option<String>, String> {
    let trimmed = code.trim_start();
    if trimmed.starts_with("? uses")
        || trimmed.starts_with("? 'uses'")
        || trimmed.starts_with("? \"uses\"")
    {
        return Err("explicit YAML mapping keys are not allowed for uses".into());
    }
    if (trimmed.starts_with('\'') || trimmed.starts_with('"'))
        && trimmed
            .split_once(':')
            .is_some_and(|(key, _)| key.contains('\\'))
    {
        return Err("escaped quoted mapping keys are not allowed in workflows".into());
    }
    let starts = ["uses", "'uses'", "\"uses\""];
    let without_dash = trimmed.strip_prefix("- ").unwrap_or(trimmed);
    let Some(prefix) = starts
        .iter()
        .find(|prefix| without_dash.starts_with(**prefix))
    else {
        // Match a YAML key token, not the suffix of permissions such as statuses.
        let embedded_uses = Regex::new(r#"(^|[^[:alnum:]_-])(uses|'uses'|"uses")\s*:"#)
            .expect("valid uses-key regex");
        if embedded_uses.is_match(code) {
            return Err("uses must be a standalone block-style step key".into());
        }
        return Ok(None);
    };
    let remainder = without_dash[prefix.len()..].trim_start();
    let Some(value) = remainder.strip_prefix(':') else {
        return Ok(None);
    };
    let value = value.trim_start();
    if value.is_empty() {
        return Err("uses must contain a reference".into());
    }
    let reference = if let Some(quote) = value
        .chars()
        .next()
        .filter(|character| matches!(character, '\'' | '"'))
    {
        let tail = &value[quote.len_utf8()..];
        let end = tail
            .find(quote)
            .ok_or_else(|| "unterminated quoted uses reference".to_string())?;
        format!("{quote}{}{quote}", &tail[..end])
    } else {
        value
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string()
    };
    Ok(Some(reference))
}

fn self_test() -> TaskResult {
    let sha = "a".repeat(40);
    if !immutable(&format!("actions/checkout@{sha}"))
        || !immutable("./.github/actions/local")
        || !immutable(&format!("docker://alpine@sha256:{}", "a".repeat(64)))
        || immutable("actions/checkout@main")
        || immutable("actions/checkout@v7")
        || immutable("docker://alpine:latest")
    {
        return Err("workflow pin self-test failed immutable-reference cases".into());
    }
    for line in [
        "      uses: actions/checkout@main",
        "      uses : 'actions/checkout@v7'",
        "      \"uses\": \"actions/checkout@main\"",
        "    - 'uses' : docker://alpine:latest # mutable image",
    ] {
        let reference =
            line_reference(line)?.ok_or_else(|| format!("self-test missed {line:?}"))?;
        if immutable(&reference) {
            return Err(format!("workflow pin self-test accepted {line:?}"));
        }
    }
    for line in [
        "    - { uses: actions/checkout@main }",
        "    - { name: checkout, 'uses': actions/checkout@main }",
        "    - { \"uses\": \"actions/checkout@main\" }",
        "    ? uses",
        r#"    "us\u0065s": actions/checkout@main"#,
    ] {
        if line_reference(line).is_ok() {
            return Err(format!(
                "workflow pin self-test accepted unsupported syntax {line:?}"
            ));
        }
    }
    for line in ["  statuses: write", "  statuses: read", "  issues: write"] {
        if line_reference(line)?.is_some() {
            return Err(format!(
                "workflow pin self-test treated permission as an action: {line:?}"
            ));
        }
    }
    if workflow_code_lines("      run: |\n        echo 'uses: actions/checkout@main'\n")
        != [(1, "      run: |".to_string())]
    {
        return Err("workflow pin self-test did not exclude a scalar body".into());
    }
    let complete_msrv = r#"
PINEWASM_FIXTURE_RUST_TOOLCHAIN="${PINEWASM_RUST_MSRV}" \
  cargo "+${PINEWASM_RUST_MSRV}" test \
    --locked -p pinewasm_xtask --all-targets
"#;
    check_msrv_xtask_lane(complete_msrv)?;
    for incomplete in [
        complete_msrv.replace(" --all-targets", " --bin pinewasm_xtask"),
        complete_msrv.replace(" --all-targets", ""),
        complete_msrv.replace(
            "PINEWASM_FIXTURE_RUST_TOOLCHAIN=\"${PINEWASM_RUST_MSRV}\"",
            "PINEWASM_FIXTURE_RUST_TOOLCHAIN=1.97.1",
        ),
    ] {
        if check_msrv_xtask_lane(&incomplete).is_ok() {
            return Err(format!(
                "workflow pin self-test accepted incomplete MSRV xtask command: {incomplete}"
            ));
        }
    }
    Ok(())
}
