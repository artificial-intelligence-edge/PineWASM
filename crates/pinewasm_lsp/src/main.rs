use pinewasm_lsp::analysis::{
    diagnostic_summary, DocumentAnalysis, InspectionStage, LibrarySource,
};
use pinewasm_lsp::workspace::Workspace;
use pinewasm_lsp::ToolResult;
use std::env;
use std::path::{Path, PathBuf};

fn main() {
    if let Err(error) = run() {
        eprintln!("pinewasm: {error}");
        std::process::exit(1);
    }
}

fn run() -> ToolResult<()> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("--version" | "-V")) {
        println!("{}", pinewasm_compiler::COMPILER_VERSION);
        return Ok(());
    }
    let workspace_override = take_workspace(&mut args)?;
    let command = args.first().map_or("lsp", String::as_str);
    match command {
        "lsp" => pinewasm_lsp::lsp::run_stdio(workspace_override),
        "check" => {
            let file = args
                .get(1)
                .ok_or("usage: pinewasm check FILE [--workspace ROOT]")?;
            let analysis = file_analysis(file, workspace_override)?;
            println!(
                "{}",
                serde_json::to_string_pretty(analysis.compile_result())?
            );
            let (errors, _) = diagnostic_summary(analysis.diagnostics());
            if errors > 0 {
                std::process::exit(1);
            }
            Ok(())
        }
        "inspect" => {
            let file = args
                .get(1)
                .ok_or("usage: pinewasm inspect FILE [STAGE] [--workspace ROOT]")?;
            let stage_name = args.get(2).map_or("all", String::as_str);
            let stage = InspectionStage::parse(stage_name)
                .ok_or("inspection stage must be ast, hir, mir, csil, or all")?;
            let analysis = file_analysis(file, workspace_override)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&analysis.inspect(stage))?
            );
            Ok(())
        }
        "migrate" => migrate_command(&args[1..]),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => Err(format!("unknown command `{other}`; run with --help").into()),
    }
}

/// `pinewasm migrate FILE [--write] [--idiomatic] [--modernize]
/// [--strict-indices] [--json]`: rewrite a Pine v5 script as v6.
fn migrate_command(args: &[String]) -> ToolResult<()> {
    use pinewasm_migrate::{migrate, MigrationOptions, Profile, Severity, Status};
    const USAGE: &str = "usage: pinewasm migrate FILE [--write] [--idiomatic] [--modernize] [--strict-indices] [--json]";
    let mut file = None;
    let mut options = MigrationOptions::default();
    let (mut write, mut json) = (false, false);
    for arg in args {
        match arg.as_str() {
            "--write" => write = true,
            "--json" => json = true,
            "--idiomatic" => options.profile = Profile::Idiomatic,
            "--modernize" => options.modernize = true,
            "--strict-indices" => options.strict_indices = true,
            flag if flag.starts_with("--") => {
                return Err(format!("unknown option `{flag}`; {USAGE}").into())
            }
            path if file.is_none() => file = Some(path.to_string()),
            _ => return Err(USAGE.into()),
        }
    }
    let file = file.ok_or(USAGE)?;
    let source = std::fs::read_to_string(&file)?;
    let result = migrate(&source, &options);
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if write {
        if result.status == Status::Migrated {
            std::fs::write(&file, &result.output)?;
        }
    } else {
        print!("{}", result.output);
    }
    if !json {
        let line_of = |offset: u32| {
            source[..(offset as usize).min(source.len())]
                .matches('\n')
                .count()
                + 1
        };
        for finding in &result.findings {
            if finding.severity == Severity::Info && !finding.rule.starts_with('D') {
                continue;
            }
            eprintln!(
                "{file}:{}: {} [{}] {}",
                line_of(finding.start),
                match finding.severity {
                    Severity::Info => "note",
                    Severity::Warning => "warning",
                    Severity::Error => "review",
                },
                finding.rule,
                finding.message
            );
        }
        for error in &result.parse_errors {
            eprintln!("{file}: parse error: {error}");
        }
    }
    match result.status {
        Status::Migrated | Status::AlreadyV6 => Ok(()),
        Status::ParseError => Err(format!("{file} does not parse; nothing was migrated").into()),
        Status::UnsupportedVersion => Err(format!(
            "{file} declares version {}; only version 5 scripts can be migrated",
            result
                .source_version
                .map_or_else(|| "1 (no version directive)".to_string(), |v| v.to_string())
        )
        .into()),
    }
}

fn take_workspace(args: &mut Vec<String>) -> ToolResult<Option<PathBuf>> {
    let Some(index) = args.iter().position(|argument| argument == "--workspace") else {
        return Ok(None);
    };
    if index + 1 >= args.len() {
        return Err("--workspace requires a path".into());
    }
    let path = PathBuf::from(args.remove(index + 1));
    args.remove(index);
    Ok(Some(path))
}

fn file_analysis(file: &str, workspace_override: Option<PathBuf>) -> ToolResult<DocumentAnalysis> {
    let file_path = PathBuf::from(file);
    let root = workspace_override.unwrap_or(env::current_dir()?);
    let workspace = Workspace::new(root)?;
    let resolved = workspace.resolve_file(Path::new(&file_path))?;
    let source = std::fs::read_to_string(&resolved)?;
    let libraries = workspace
        .libraries(&std::collections::HashMap::default())
        .into_iter()
        .enumerate()
        .map(|(index, (import_path, source))| LibrarySource {
            file_name: workspace
                .library_file(index as u32 + 1)
                .map(|library| library.file.display().to_string()),
            import_path,
            source,
        })
        .collect();
    Ok(DocumentAnalysis::new(
        resolved.display().to_string(),
        source,
        libraries,
    ))
}

fn print_help() {
    println!(
        "pinewasm {version}\n\n\
         USAGE:\n  pinewasm lsp [--workspace ROOT]\n  \
         pinewasm check FILE [--workspace ROOT]\n  \
         pinewasm inspect FILE [ast|hir|mir|csil|all] [--workspace ROOT]\n  \
         pinewasm migrate FILE [--write] [--idiomatic] [--modernize] [--strict-indices] [--json]\n",
        version = pinewasm_compiler::COMPILER_VERSION
    );
}
