//! `standalone_eligibility` against the standalone WebAssembly compile.
//!
//! The eligibility function is the part of the compile that runs before a
//! byte is emitted, and the compile calls it. So the two cannot disagree on a
//! refusal the function reports. What this file measures is the other side:
//! whether the compile ever refuses a program the function accepted, which
//! would be a refusal first raised during emission or by the audit of the
//! emitted bytes.
//!
//! The corpus is every cell of the typed matrix (`typed_matrix/cells.rs`) and
//! every fixture under `tests/compile-pass` that the front end compiles (the enumeration of
//! `standalone_wasm_census.rs`). Each program is built as a strategy-type
//! module and as an indicator-type module. The fixtures are also built with
//! the signal slice on.
//!
//! Three statements are asserted over that corpus:
//!
//! * agreement: eligibility is `Ok` exactly when the compile is `Ok`, and when
//!   both refuse they return the same error;
//! * nothing is first refused during emission: the list of programs accepted
//!   by eligibility and refused by the compile is [`EMISSION_REFUSALS`];
//! * every refusal is of the capability class.

use pinewasm_aot::{
    compile_standalone_wasm_program_explained, standalone_eligibility, AotError, RefusalClass,
    StandaloneCompileRefusal, StandaloneRefusal, StandaloneRefusalSite, StandaloneWasmBuildConfig,
    StandaloneWasmModuleType, StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_csil::{CsilInstr, CsilProgram, F64Bits};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[path = "typed_matrix/cells.rs"]
#[allow(dead_code)]
mod cells;
use cells::SIGNAL_TITLE;

/// Programs that eligibility accepts and the compile then refuses, as
/// `(corpus, build, program, kind)`. Empty: over the corpus, no refusal is
/// first raised during emission or by the audit.
const EMISSION_REFUSALS: &[(&str, &str, &str, &str)] = &[];

/// Programs per corpus that reach the backend, and per build how many the
/// compile accepts. A change here is a change in the corpus or in what is
/// accepted, and is reviewed in the typed matrix grid and the census first.
///
/// The matrix has 8,227 cells, of which the front end rejects 46. The
/// indicator count over the fixtures is three above the census's 31 because
/// the census leaves `strategy()` sources out of its indicator column, and
/// here every program is built both ways.
const PINNED_COUNTS: &[(&str, &str, usize, usize)] = &[
    // (corpus, build, programs, accepted)
    ("matrix", "strategy", 8181, 7835),
    ("matrix", "indicator", 8181, 7835),
    ("fixtures", "strategy", 51, 12),
    ("fixtures", "indicator", 51, 34),
    ("fixtures", "strategy+slice", 51, 12),
    ("fixtures", "indicator+slice", 51, 34),
];

struct Build {
    name: &'static str,
    module_type: StandaloneWasmModuleType,
    signal_slice: bool,
}

const MATRIX_BUILDS: &[Build] = &[
    Build {
        name: "strategy",
        module_type: StandaloneWasmModuleType::Strategy,
        signal_slice: false,
    },
    Build {
        name: "indicator",
        module_type: StandaloneWasmModuleType::Indicator,
        signal_slice: false,
    },
];

const FIXTURE_BUILDS: &[Build] = &[
    Build {
        name: "strategy",
        module_type: StandaloneWasmModuleType::Strategy,
        signal_slice: false,
    },
    Build {
        name: "indicator",
        module_type: StandaloneWasmModuleType::Indicator,
        signal_slice: false,
    },
    Build {
        name: "strategy+slice",
        module_type: StandaloneWasmModuleType::Strategy,
        signal_slice: true,
    },
    Build {
        name: "indicator+slice",
        module_type: StandaloneWasmModuleType::Indicator,
        signal_slice: true,
    },
];

/// How eligibility and the compile compare on one program and one build.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    BothAccept,
    /// Both refuse with the same error.
    BothRefuse {
        kind: &'static str,
        class: RefusalClass,
    },
    /// Both refuse, with different errors.
    DifferentRefusals(String),
    /// Eligibility accepts and the compile refuses.
    RefusedInEmission {
        kind: &'static str,
        class: RefusalClass,
    },
    /// Eligibility refuses and the compile accepts.
    RefusedOnlyByEligibility(String),
}

fn verdict(eligibility: Result<(), StandaloneRefusal>, compiled: Result<(), AotError>) -> Verdict {
    match (eligibility, compiled) {
        (Ok(()), Ok(())) => Verdict::BothAccept,
        (Ok(()), Err(error)) => Verdict::RefusedInEmission {
            kind: error.kind(),
            class: error.class(),
        },
        (Err(refusal), Ok(())) => Verdict::RefusedOnlyByEligibility(refusal.to_string()),
        (Err(refusal), Err(error)) => {
            let reason = refusal.reason();
            let message = refusal.to_string();
            let same = reason.kind == error.kind()
                && reason.class == error.class()
                && message == error.to_string()
                && refusal.as_aot_error() == &error
                && AotError::from(refusal) == error;
            if same {
                Verdict::BothRefuse {
                    kind: error.kind(),
                    class: error.class(),
                }
            } else {
                Verdict::DifferentRefusals(format!(
                    "eligibility {} ({message}), compile {} ({error})",
                    reason.kind,
                    error.kind()
                ))
            }
        }
    }
}

fn compare(
    program: &CsilProgram,
    opts: &StandaloneWasmOptions,
    config: &StandaloneWasmBuildConfig,
) -> Verdict {
    let eligibility = standalone_eligibility(program, opts, config);
    let explained = compile_standalone_wasm_program_explained(program, opts, config);
    // The refusal a failed compile hands over is the answer to the second
    // eligibility run a caller no longer makes: present exactly when
    // eligibility refuses with the compile's error, and then that refusal.
    if let Err(refused) = &explained {
        let asked = eligibility
            .as_ref()
            .err()
            .filter(|refusal| refusal.as_aot_error() == refused.error());
        assert_eq!(refused.eligibility(), asked);
    }
    verdict(
        eligibility,
        explained
            .map(drop)
            .map_err(StandaloneCompileRefusal::into_error),
    )
}

/// The front end's output for `source`, or `None` when it rejects the source.
fn front_end(source: &str) -> Option<CsilProgram> {
    let result = compile(source, CompileOptions::default());
    if result
        .diagnostics
        .iter()
        .any(pinewasm_span::Diagnostic::is_error)
    {
        return None;
    }
    result.program
}

struct Row {
    corpus: &'static str,
    build: &'static str,
    program: String,
    verdict: Verdict,
}

fn config(build: &Build) -> StandaloneWasmBuildConfig {
    StandaloneWasmBuildConfig {
        signal_slice: build.signal_slice,
        ..StandaloneWasmBuildConfig::default()
    }
}

/// A matrix cell in every matrix build. The strategy build designates the
/// cell's signal plot, as the typed matrix does.
fn matrix_rows(cell: &cells::Cell) -> Vec<Row> {
    let Some(program) = front_end(&cell.program) else {
        return Vec::new();
    };
    let signal_plot = program
        .plots
        .iter()
        .position(|plot| plot.title.as_deref() == Some(SIGNAL_TITLE));
    assert!(signal_plot.is_some(), "{}: no signal plot", cell.id);
    MATRIX_BUILDS
        .iter()
        .map(|build| {
            let opts = StandaloneWasmOptions {
                signal_plot: match build.module_type {
                    StandaloneWasmModuleType::Strategy => signal_plot,
                    StandaloneWasmModuleType::Indicator => None,
                },
                module_type: build.module_type,
            };
            Row {
                corpus: "matrix",
                build: build.name,
                program: cell.id.clone(),
                verdict: compare(&program, &opts, &config(build)),
            }
        })
        .collect()
}

fn collect_pine_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("readable fixture directory") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|name| name == "generated_failures")
            {
                continue;
            }
            collect_pine_files(&path, out);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "pine")
        {
            out.push(path);
        }
    }
}

/// A fixture in every fixture build, with the default signal designation the
/// census uses.
fn fixture_rows(path: &Path) -> Vec<Row> {
    let source =
        std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let Some(program) = front_end(&source) else {
        return Vec::new();
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    FIXTURE_BUILDS
        .iter()
        .map(|build| {
            let opts = StandaloneWasmOptions {
                signal_plot: None,
                module_type: build.module_type,
            };
            Row {
                corpus: "fixtures",
                build: build.name,
                program: name.clone(),
                verdict: compare(&program, &opts, &config(build)),
            }
        })
        .collect()
}

/// `rows_of` over every item, split across threads. The items are
/// independent, and the rows come back in item order.
fn in_parallel<T: Sync>(items: &[T], rows_of: impl Fn(&T) -> Vec<Row> + Sync) -> Vec<Row> {
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(8);
    let mut rows: Vec<Option<Vec<Row>>> = (0..items.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|worker| {
                let rows_of = &rows_of;
                scope.spawn(move || {
                    items
                        .iter()
                        .enumerate()
                        .skip(worker)
                        .step_by(workers)
                        .map(|(index, item)| (index, rows_of(item)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for handle in handles {
            for (index, item_rows) in handle.join().expect("worker") {
                rows[index] = Some(item_rows);
            }
        }
    });
    rows.into_iter()
        .flat_map(|item_rows| item_rows.expect("every item compared"))
        .collect()
}

/// Every row of the corpus, computed once for the three statements.
fn corpus() -> &'static [Row] {
    static ROWS: OnceLock<Vec<Row>> = OnceLock::new();
    ROWS.get_or_init(|| {
        let mut rows = in_parallel(&cells::cells(), matrix_rows);
        let mut files = Vec::new();
        collect_pine_files(Path::new("../../tests/compile-pass"), &mut files);
        files.sort();
        rows.extend(in_parallel(&files, |path| fixture_rows(path)));
        rows
    })
}

#[test]
fn eligibility_and_the_compile_agree_on_every_program() {
    let rows = corpus();
    let mut disagreements = Vec::new();
    let mut counts: BTreeMap<(&str, &str), (usize, usize)> = BTreeMap::new();
    let mut refusals: BTreeMap<(&str, &str, &str), usize> = BTreeMap::new();
    for row in rows {
        let entry = counts.entry((row.corpus, row.build)).or_default();
        entry.0 += 1;
        match &row.verdict {
            Verdict::BothAccept => entry.1 += 1,
            Verdict::BothRefuse { kind, .. } => {
                *refusals.entry((row.corpus, row.build, kind)).or_default() += 1;
            }
            // Counted by `nothing_is_first_refused_during_emission`.
            Verdict::RefusedInEmission { .. } => {}
            Verdict::DifferentRefusals(detail) | Verdict::RefusedOnlyByEligibility(detail) => {
                disagreements.push(format!(
                    "{} {} {}: {detail}",
                    row.corpus, row.build, row.program
                ));
            }
        }
    }
    for ((corpus, build), (programs, accepted)) in &counts {
        eprintln!(
            "[standalone-eligibility] {corpus:<8} {build:<16} programs={programs} \
             accepted={accepted}"
        );
    }
    for ((corpus, build, kind), count) in &refusals {
        eprintln!("[standalone-eligibility] {corpus:<8} {build:<16} {kind:<36} {count}");
    }
    assert!(
        disagreements.is_empty(),
        "eligibility and the compile return different refusals:\n{}",
        disagreements.join("\n")
    );
    let measured: Vec<(&str, &str, usize, usize)> = counts
        .iter()
        .map(|(&(corpus, build), &(programs, accepted))| (corpus, build, programs, accepted))
        .collect();
    let mut pinned = PINNED_COUNTS.to_vec();
    pinned.sort_unstable();
    assert_eq!(
        measured, pinned,
        "the corpus or what it accepts changed; review the typed matrix grid and the census, \
         then update PINNED_COUNTS"
    );
}

#[test]
fn nothing_is_first_refused_during_emission() {
    let measured: Vec<(&str, &str, &str, &str)> = corpus()
        .iter()
        .filter_map(|row| match &row.verdict {
            Verdict::RefusedInEmission { kind, .. } => {
                Some((row.corpus, row.build, row.program.as_str(), *kind))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        measured, EMISSION_REFUSALS,
        "the programs that eligibility accepts and the compile refuses changed; each one is a \
         refusal first raised during emission or by the audit"
    );
}

#[test]
fn every_refusal_is_a_capability_refusal() {
    let internal: Vec<String> = corpus()
        .iter()
        .filter_map(|row| match &row.verdict {
            Verdict::BothRefuse { kind, class } | Verdict::RefusedInEmission { kind, class }
                if *class != RefusalClass::Capability =>
            {
                Some(format!(
                    "{} {} {}: {kind}",
                    row.corpus, row.build, row.program
                ))
            }
            _ => None,
        })
        .collect();
    assert!(
        internal.is_empty(),
        "front-end output was refused for an internal reason; this is a compiler defect:\n{}",
        internal.join("\n")
    );
}

// ---------------------------------------------------------------------------
// Controls and the structured reason.

/// The comparison reports a compile refusal that eligibility did not see, and
/// two refusals that differ. Without this the corpus tests could pass by
/// never producing those verdicts.
#[test]
fn the_comparison_reports_an_emission_refusal_and_a_different_refusal() {
    assert_eq!(
        verdict(
            Ok(()),
            Err(AotError::UnsupportedOpcode("PineIntDiv".into()))
        ),
        Verdict::RefusedInEmission {
            kind: "UnsupportedOpcode",
            class: RefusalClass::Capability,
        }
    );
    let refused = || StandaloneRefusal::from(AotError::UnsupportedOpcode("PineIntDiv".into()));
    assert!(matches!(
        verdict(Err(refused()), Ok(())),
        Verdict::RefusedOnlyByEligibility(_)
    ));
    assert!(matches!(
        verdict(
            Err(refused()),
            Err(AotError::UnsupportedOpcode("WindowMeanDynamic".into()))
        ),
        Verdict::DifferentRefusals(_)
    ));
    assert_eq!(
        verdict(
            Err(refused()),
            Err(AotError::UnsupportedOpcode("PineIntDiv".into()))
        ),
        Verdict::BothRefuse {
            kind: "UnsupportedOpcode",
            class: RefusalClass::Capability,
        }
    );
}

const CONTROL: &str = r#"//@version=6
indicator("eligibility control")
var int n = 0
n := 5
plot(close, "Backtest Signal")
"#;

fn control() -> CsilProgram {
    front_end(CONTROL).expect("the control source compiles")
}

fn refusal(
    program: &CsilProgram,
    opts: &StandaloneWasmOptions,
    config: &StandaloneWasmBuildConfig,
) -> StandaloneRefusal {
    let verdict = compare(program, opts, config);
    assert!(
        matches!(verdict, Verdict::BothRefuse { .. }),
        "expected the same refusal from both, got {verdict:?}"
    );
    standalone_eligibility(program, opts, config).expect_err("refused")
}

const STRATEGY: StandaloneWasmOptions = StandaloneWasmOptions {
    signal_plot: None,
    module_type: StandaloneWasmModuleType::Strategy,
};

#[test]
fn the_control_program_is_eligible() {
    let default = StandaloneWasmBuildConfig::default();
    assert_eq!(
        compare(&control(), &STRATEGY, &default),
        Verdict::BothAccept
    );
}

/// An internal refusal is reported by eligibility as internal: the capability
/// statement over the corpus is not true by construction. Same planted
/// program as `refusal_classes.rs`.
#[test]
fn a_planted_type_conflict_is_an_internal_refusal() {
    let mut program = control();
    let function = &mut program.functions[0];
    let slot = function
        .code
        .iter()
        .find_map(|instr| match instr {
            CsilInstr::StoreUserSlotCurrent { slot } => Some(*slot),
            _ => None,
        })
        .expect("the control source stores a user slot");
    let at = function.code.len() - 1;
    function.code.splice(
        at..at,
        [
            CsilInstr::PushF64(F64Bits::new(1.5)),
            CsilInstr::StoreUserSlotCurrent { slot },
        ],
    );
    let span = function.source_map[at];
    function.source_map.splice(at..at, [span, span]);
    // The edit no longer matches the stored budget reference.
    function.budget_reference = None;
    pinewasm_csil::validate(&program).expect("the validator accepts the planted program");

    let refusal = refusal(&program, &STRATEGY, &StandaloneWasmBuildConfig::default());
    assert_eq!(refusal.kind(), "TypeConflict");
    assert_eq!(refusal.class(), RefusalClass::Internal);
    assert_eq!(refusal.site(), None);
}

#[test]
fn a_build_option_refusal_has_no_site() {
    for max_collection_elements in [0, pinewasm_aot::STANDALONE_WASM_MAX_COLLECTION_ELEMENTS + 1] {
        let config = StandaloneWasmBuildConfig {
            max_collection_elements,
            ..StandaloneWasmBuildConfig::default()
        };
        let refusal = refusal(&control(), &STRATEGY, &config);
        assert_eq!(refusal.kind(), "StandaloneWasmInvalidOptions");
        assert_eq!(refusal.class(), RefusalClass::Capability);
        assert_eq!(refusal.site(), None);
    }
}

#[test]
fn a_plot_refusal_names_the_plot_the_caller_passed() {
    let default = StandaloneWasmBuildConfig::default();
    for (module_type, index) in [
        // Out of range for a one-plot program.
        (StandaloneWasmModuleType::Strategy, 7),
        // An indicator-type module designates no plot.
        (StandaloneWasmModuleType::Indicator, 0),
    ] {
        let opts = StandaloneWasmOptions {
            signal_plot: Some(index),
            module_type,
        };
        let refusal = refusal(&control(), &opts, &default);
        assert_eq!(refusal.kind(), "StandaloneWasmNoDesignatedPlot");
        assert_eq!(refusal.site(), Some(StandaloneRefusalSite::Plot { index }));
    }

    // No plot was passed and the program does not have exactly one.
    let two_plots = front_end(
        "//@version=6\nindicator(\"two plots\")\nplot(close, \"A\")\nplot(open, \"B\")\n",
    )
    .expect("compiles");
    let refusal = refusal(&two_plots, &STRATEGY, &default);
    assert_eq!(refusal.kind(), "StandaloneWasmNoDesignatedPlot");
    assert_eq!(refusal.site(), None);
}

#[test]
fn a_source_input_refusal_names_the_input() {
    let program = front_end(
        "//@version=6\nindicator(\"derived source\")\nn = input.int(3, \"N\")\n\
         src = input.source(hl2, \"Source\")\nplot(src + n, \"Backtest Signal\")\n",
    )
    .expect("compiles");
    let refusal = refusal(&program, &STRATEGY, &StandaloneWasmBuildConfig::default());
    assert_eq!(refusal.kind(), "StandaloneWasmSourceInputUnsupported");
    assert_eq!(refusal.site(), Some(StandaloneRefusalSite::Input { id: 1 }));
}

#[test]
fn a_host_site_refusal_names_the_first_surviving_host_site() {
    let program = front_end(
        "//@version=6\nindicator(\"host site\")\ns = input.string(\"a\", \"S\")\n\
         plot(s == \"a\" ? close : open, \"Backtest Signal\")\n",
    )
    .expect("compiles");
    let refusal = refusal(&program, &STRATEGY, &StandaloneWasmBuildConfig::default());
    assert_eq!(refusal.kind(), "StandaloneWasmNotStandalone");
    let Some(StandaloneRefusalSite::HostSite { pc }) = refusal.site() else {
        panic!("expected a host site, got {:?}", refusal.site());
    };
    assert!(
        matches!(program.functions[0].code[pc], CsilInstr::ReadInput { .. }),
        "pc {pc} is {:?}",
        program.functions[0].code[pc]
    );
}

#[test]
fn unsliced_refusal_after_a_search_guard_names_the_supplied_instruction() {
    let source = format!("{}\nmarker = input.string(\"a\", \"Marker\")\nplot(marker == \"a\" ? close : open, \"Host value\")\n", include_str!("../../../tests/regression/indexed_distance_scan.pine"));
    let p = front_end(&source).unwrap();
    let config = StandaloneWasmBuildConfig {
        signal_slice: false,
        ..Default::default()
    };
    let options = StandaloneWasmOptions {
        signal_plot: Some(4),
        module_type: StandaloneWasmModuleType::Strategy,
    };
    let r = refusal(&p, &options, &config);
    assert_eq!(r.kind(), "StandaloneWasmNotStandalone");
    let Some(StandaloneRefusalSite::HostSite { pc }) = r.site() else {
        panic!("expected a host site: {:?}", r.site());
    };
    assert!(
        matches!(p.main().code[pc], CsilInstr::ReadInput { .. }),
        "{pc}: {:?}",
        p.main().code[pc]
    );
}
