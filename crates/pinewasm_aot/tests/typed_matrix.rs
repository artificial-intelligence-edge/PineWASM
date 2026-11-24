//! The typed matrix: programs for this front end, generated from PineWASM's own
//! tables of Pine Script® constructs, each one built as a standalone WebAssembly
//! module and, when a module is produced, run against the interpreter.
//!
//! The VM/AOT differential generator in `differential.rs` stays inside what
//! the backend already lowers, so it cannot find a valid script the backend
//! wrongly refuses. This generator does not ask the backend anything. It
//! crosses three tables (in `typed_matrix/cells.rs`):
//!
//! * [`SOURCES`]: where a value comes from (a literal, an input, a series, a
//!   slot, an object field, an array element, a function result, ...), for
//!   `int`, `float` and `bool`;
//! * [`CONSUMERS`]: what reads the value (every numeric `math.*` builtin,
//!   arithmetic, comparisons, casts, `nz`, `na`, a `ta.*` argument, a history
//!   index, ...), with the type it accepts and the type it returns;
//! * the result contexts ([`Context`]): bound to an `int` and widened in
//!   float arithmetic, bound to a `float`, bound to a `bool`, or used
//!   directly.
//!
//! A second layer applies a consumer to a consumer over the fixed subsets
//! [`DEPTH2_SOURCES`] and [`DEPTH2_CONSUMERS`]. There is no randomness: the
//! cell list is a pure function of the tables, so adding a builtin is one
//! line and one block of new grid lines.
//!
//! Every cell gets exactly one outcome:
//!
//! | Outcome | Meaning |
//! |---|---|
//! | `ok` | a module was produced and every plot lane is bit-equal to the interpreter's on every bar, na included |
//! | `ok:runtime-error` | a module was produced and the interpreter and the module both stop with a runtime error |
//! | a refusal kind, e.g. `UnsupportedIntegerNaEncoding` | the backend declined for a declared capability limit ([`RefusalClass::Capability`]) |
//! | `frontend:<code>` | the front end rejects the program; the generator produced something the front end does not accept |
//! | `known-defect:<name>` | a compiler defect listed in [`KNOWN_DEFECTS`] |
//!
//! An internal refusal ([`RefusalClass::Internal`]), a validator rejection of
//! front-end output, and any disagreement between the interpreter and a
//! module are compiler defects. They fail the test with the program text
//! unless the cell is listed in [`KNOWN_DEFECTS`].
//!
//! The outcomes are pinned in `typed_matrix/grid.rs`, one line per cell plus one
//! total per outcome. The test fails with one message when a cell pinned as
//! accepted is no longer accepted (an acceptance regression) and with another
//! for any other difference.
//!
//! ```text
//! cargo test -p pinewasm_aot --test typed_matrix -- --nocapture
//! TYPED_MATRIX_BLESS=1 cargo test -p pinewasm_aot --test typed_matrix   # rewrite the grid
//! TYPED_MATRIX_DUMP=/tmp/cells.json cargo test -p pinewasm_aot --test typed_matrix
//! ```
//!
//! The self-tests at the end of the file run on every build.
//!
//! `TYPED_MATRIX_DUMP` writes every generated source with its cell id as a
//! JSON array of `{"id", "source"}`, so the generated programs can be reused
//! in other test runs.

use pinewasm_aot::{
    compile_standalone_wasm_program, RefusalClass, StandaloneWasmModuleType, StandaloneWasmOptions,
};
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_mir::Value;
use pinewasm_vm::{run, MarketData, RunOptions};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

mod support;
use support::{standalone_wasm_wire, synthetic_data, StandaloneWasmHost};

/// The pinned grid, stored as a Rust source file.
#[path = "typed_matrix/grid.rs"]
mod grid;

/// The generation tables and the cell enumeration, shared with
/// `emitter_byte_gate.rs`.
#[path = "typed_matrix/cells.rs"]
mod cells;
use cells::{cells, Cell, SIGNAL_TITLE};

/// Bars in the synthetic feed every accepted cell runs on.
const BARS: usize = 48;

// ---------------------------------------------------------------------------
// One cell.

/// What happened to one program. `Defect` carries the evidence; every other
/// variant is a legitimate, pinned answer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Ok,
    OkRuntimeError,
    FrontEnd(String),
    Refused(&'static str),
    Defect(String),
}

fn compare_lanes(
    outcome: &pinewasm_vm::RunOutcome,
    host: &mut StandaloneWasmHost,
    bars: usize,
) -> Result<(), String> {
    if outcome.plots.is_empty() {
        return Err("the interpreter produced no plots".into());
    }
    for plot in &outcome.plots {
        let lane = host.plot_lane(plot.slot as usize);
        if lane.len() != bars || plot.values.len() != bars {
            return Err(format!(
                "plot {}: {} interpreter bars, {} module bars, {bars} fed",
                plot.slot,
                plot.values.len(),
                lane.len()
            ));
        }
        for (bar, (expected, got)) in plot.values.iter().zip(&lane).enumerate() {
            let agree = match expected {
                Value::Na => got.is_nan(),
                Value::Float(x) => x.to_bits() == got.to_bits(),
                _ => false,
            };
            if !agree {
                return Err(format!(
                    "plot {} bar {bar}: interpreter {expected:?}, module {got:?} ({:#018x})",
                    plot.slot,
                    got.to_bits()
                ));
            }
        }
    }
    Ok(())
}

fn evaluate(source: &str, data: &MarketData, wire: &str) -> Outcome {
    let result = compile(source, CompileOptions::all());
    if let Some(error) = result
        .diagnostics
        .iter()
        .find(|d| pinewasm_span::Diagnostic::is_error(d))
    {
        return Outcome::FrontEnd(error.code.clone().unwrap_or_else(|| "internal".to_owned()));
    }
    let Some(program) = result.program else {
        return Outcome::Defect("the front end reported no error and produced no program".into());
    };
    match build(&program) {
        Ok(wasm) => differ(&program, &wasm, data, wire),
        Err(outcome) => outcome,
    }
}

/// Validate front-end output and build the standalone module with the
/// designated plot. A capability refusal is an answer; a validator rejection
/// or an internal refusal is a defect.
fn build(program: &pinewasm_csil::CsilProgram) -> Result<Vec<u8>, Outcome> {
    if let Err(errors) = pinewasm_csil::validate(program) {
        return Err(Outcome::Defect(format!(
            "the validator rejects front-end output: {errors:?}"
        )));
    }
    let Some(signal_plot) = program
        .plots
        .iter()
        .position(|plot| plot.title.as_deref() == Some(SIGNAL_TITLE))
    else {
        return Err(Outcome::Defect(format!(
            "the program has no \"{SIGNAL_TITLE}\" plot"
        )));
    };
    compile_standalone_wasm_program(
        program,
        &StandaloneWasmOptions {
            signal_plot: Some(signal_plot),
            module_type: StandaloneWasmModuleType::Strategy,
        },
    )
    .map_err(|error| match error.class() {
        RefusalClass::Capability => Outcome::Refused(error.kind()),
        RefusalClass::Internal => {
            Outcome::Defect(format!("internal refusal {}: {error}", error.kind()))
        }
    })
}

/// Run the interpreter on `program` and the module on the same bars, and
/// require the same answer.
fn differ(
    program: &pinewasm_csil::CsilProgram,
    wasm: &[u8],
    data: &MarketData,
    wire: &str,
) -> Outcome {
    let reference = run(program, data, &RunOptions::default());
    let mut host = StandaloneWasmHost::new(wasm);
    match (reference.errors.is_empty(), host.evaluate(wire)) {
        (true, Ok(_)) => match compare_lanes(&reference, &mut host, data.bars()) {
            Ok(()) => Outcome::Ok,
            Err(difference) => Outcome::Defect(format!("wrong value: {difference}")),
        },
        (false, Err(_)) => Outcome::OkRuntimeError,
        (true, Err(message)) => Outcome::Defect(format!(
            "the interpreter completes and the module fails: {message}"
        )),
        (false, Ok(_)) => Outcome::Defect(format!(
            "the interpreter fails and the module completes: {:?}",
            reference.errors
        )),
    }
}

/// [`evaluate`], with a panic anywhere in the pipeline reported as a defect
/// of that cell instead of aborting the whole matrix.
fn evaluate_caught(source: &str, data: &MarketData, wire: &str) -> Outcome {
    catch_unwind(AssertUnwindSafe(|| evaluate(source, data, wire))).unwrap_or_else(|payload| {
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_else(|| "non-string panic".to_owned());
        Outcome::Defect(format!("panic: {message}"))
    })
}

/// Every cell's outcome, in cell order. The cells are independent, so they
/// are split across threads; the result does not depend on the split.
fn evaluate_all(cells: &[Cell]) -> Vec<Outcome> {
    let data = synthetic_data(BARS);
    let wire = standalone_wasm_wire(&data, None);
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(8);
    let mut outcomes: Vec<Option<Outcome>> = vec![None; cells.len()];
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|worker| {
                let (data, wire) = (&data, &wire);
                scope.spawn(move || {
                    cells
                        .iter()
                        .enumerate()
                        .skip(worker)
                        .step_by(workers)
                        .map(|(index, cell)| (index, evaluate_caught(&cell.program, data, wire)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for handle in handles {
            for (index, outcome) in handle.join().expect("worker") {
                outcomes[index] = Some(outcome);
            }
        }
    });
    outcomes
        .into_iter()
        .map(|outcome| outcome.expect("every cell evaluated"))
        .collect()
}

// ---------------------------------------------------------------------------
// Known defects.

/// A compiler defect this matrix found and that is not fixed yet. The cell
/// stays in the grid as `known-defect:<name>`; when the defect is fixed the
/// cell stops being defective and the test asks for the entry to be removed.
struct KnownDefect {
    name: &'static str,
    /// What is wrong, for the reader of a failure.
    what: &'static str,
    /// The affected cell ids.
    cells: &'static [&'static str],
}

const KNOWN_DEFECTS: &[KnownDefect] = &[];

fn known_defect(cell: &str) -> Option<&'static KnownDefect> {
    KNOWN_DEFECTS
        .iter()
        .find(|defect| defect.cells.contains(&cell))
}

// ---------------------------------------------------------------------------
// The pinned grid.

fn grid_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/typed_matrix/grid.rs")
}

/// The Rust source file that holds `grid` as [`grid::PINNED`].
fn render_grid_source(grid: &[(String, String)]) -> String {
    let text = render_grid(grid);
    assert!(!text.contains('"'), "a cell id would end the raw string");
    format!(
        "//! The pinned typed matrix outcome grid. Generated by `tests/typed_matrix.rs`; do not\n\
         //! edit by hand. Rewrite with:\n\
         //! `TYPED_MATRIX_BLESS=1 cargo test -p pinewasm_aot --test typed_matrix`\n\n\
         pub const PINNED: &str = r\"{text}\";\n"
    )
}

/// Whether a grid value means "a module was produced and it is right".
fn accepted(value: &str) -> bool {
    value == "ok" || value == "ok:runtime-error"
}

const GRID_HEADER: &str = "\
# Cell lines: <source>|<consumer>|<context><TAB><outcome>. Total lines: @total<TAB><outcome><TAB><count>.
";

fn totals(grid: &[(String, String)]) -> BTreeMap<&str, usize> {
    let mut totals = BTreeMap::new();
    for (_, value) in grid {
        *totals.entry(value.as_str()).or_default() += 1;
    }
    totals
}

fn render_grid(grid: &[(String, String)]) -> String {
    let mut out = String::from(GRID_HEADER);
    for (value, count) in totals(grid) {
        let _ = writeln!(out, "@total\t{value}\t{count}");
    }
    for (id, value) in grid {
        let _ = writeln!(out, "{id}\t{value}");
    }
    out
}

/// The cell lines of a grid file, in file order. Total lines are derived
/// from the cells, so they are checked by re-rendering, not parsed.
fn parse_grid(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter(|line| !line.starts_with('#') && !line.starts_with("@total\t"))
        .map(|line| {
            let (id, value) = line
                .split_once('\t')
                .unwrap_or_else(|| panic!("malformed grid line: {line:?}"));
            (id.to_owned(), value.to_owned())
        })
        .collect()
}

/// How a measured grid differs from the pinned one.
#[derive(Debug, Default, PartialEq, Eq)]
struct GridDifference {
    /// Cells pinned as accepted that are no longer accepted.
    acceptance_regressions: Vec<String>,
    /// Every other difference: a refusal that changed kind, a cell newly
    /// accepted, a cell added to or removed from the matrix.
    other: Vec<String>,
}

fn compare_grids(pinned: &[(String, String)], measured: &[(String, String)]) -> GridDifference {
    let pinned_by_id: BTreeMap<&str, &str> = pinned
        .iter()
        .map(|(id, value)| (id.as_str(), value.as_str()))
        .collect();
    let measured_by_id: BTreeMap<&str, &str> = measured
        .iter()
        .map(|(id, value)| (id.as_str(), value.as_str()))
        .collect();
    let mut difference = GridDifference::default();
    for (id, value) in measured {
        match pinned_by_id.get(id.as_str()) {
            Some(was) if was == value => {}
            Some(was) if accepted(was) && !accepted(value) => difference
                .acceptance_regressions
                .push(format!("{id}: was {was}, now {value}")),
            Some(was) => difference
                .other
                .push(format!("{id}: was {was}, now {value}")),
            None => difference
                .other
                .push(format!("{id}: not in the grid, now {value}")),
        }
    }
    for (id, was) in pinned {
        if !measured_by_id.contains_key(id.as_str()) {
            difference
                .other
                .push(format!("{id}: was {was}, no longer generated"));
        }
    }
    if difference == GridDifference::default() && pinned != measured {
        difference.other.push("the cell order changed".to_owned());
    }
    difference
}

// ---------------------------------------------------------------------------
// The tests.

#[test]
fn typed_matrix_matches_the_pinned_grid_and_the_interpreter() {
    let cells = cells();
    let mut ids = std::collections::BTreeSet::new();
    for cell in &cells {
        assert!(
            ids.insert(cell.id.as_str()),
            "duplicate cell id {}",
            cell.id
        );
    }
    for defect in KNOWN_DEFECTS {
        for cell in defect.cells {
            assert!(
                ids.contains(cell),
                "KNOWN_DEFECTS entry {} names a cell that is not generated: {cell}",
                defect.name
            );
        }
    }

    if let Ok(path) = std::env::var("TYPED_MATRIX_DUMP") {
        let dump: Vec<serde_json::Value> = cells
            .iter()
            .map(|cell| serde_json::json!({ "id": cell.id, "source": cell.program }))
            .collect();
        std::fs::write(&path, serde_json::to_string_pretty(&dump).expect("JSON"))
            .unwrap_or_else(|error| panic!("write {path}: {error}"));
        println!("[typed-matrix] wrote {} programs to {path}", cells.len());
    }

    let outcomes = evaluate_all(&cells);

    let mut defects: Vec<String> = Vec::new();
    let mut stale: Vec<String> = Vec::new();
    let mut grid: Vec<(String, String)> = Vec::with_capacity(cells.len());
    for (cell, outcome) in cells.iter().zip(&outcomes) {
        let known = known_defect(&cell.id);
        let value = match outcome {
            Outcome::Ok => "ok".to_owned(),
            Outcome::OkRuntimeError => "ok:runtime-error".to_owned(),
            Outcome::FrontEnd(code) => format!("frontend:{code}"),
            Outcome::Refused(kind) => (*kind).to_owned(),
            Outcome::Defect(evidence) => match known {
                Some(defect) => format!("known-defect:{}", defect.name),
                None => {
                    defects.push(format!("{}: {evidence}\n{}", cell.id, cell.program));
                    "defect".to_owned()
                }
            },
        };
        if let (Some(defect), false) = (known, matches!(outcome, Outcome::Defect(_))) {
            stale.push(format!(
                "{} ({}: {}) is now {value}",
                cell.id, defect.name, defect.what
            ));
        }
        grid.push((cell.id.clone(), value));
    }

    println!("[typed-matrix] {} cells", grid.len());
    for (value, count) in totals(&grid) {
        println!("[typed-matrix] {count:>6}  {value}");
    }

    assert!(
        defects.is_empty(),
        "{} compiler defect(s): an internal refusal of front-end output, or a module that \
         disagrees with the interpreter. Fix the compiler, or list the cell in KNOWN_DEFECTS \
         with a name and report it.\n\n{}",
        defects.len(),
        defects.join("\n")
    );
    assert!(
        stale.is_empty(),
        "KNOWN_DEFECTS lists cells that are no longer defective; remove them and bless the \
         grid:\n{}",
        stale.join("\n")
    );

    let path = grid_path();
    if std::env::var_os("TYPED_MATRIX_BLESS").is_some() {
        std::fs::write(&path, render_grid_source(&grid))
            .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
        println!("[typed-matrix] rewrote {}", path.display());
        return;
    }
    let difference = compare_grids(&parse_grid(grid::PINNED), &grid);
    assert!(
        difference.acceptance_regressions.is_empty(),
        "ACCEPTANCE REGRESSION: {} program(s) the pinned grid accepts are no longer accepted. \
         This build refuses or mishandles programs that the grid accepts; do not bless \
         this without reviewing each one.\n{}",
        difference.acceptance_regressions.len(),
        difference.acceptance_regressions.join("\n")
    );
    assert!(
        difference.other.is_empty(),
        "the typed matrix outcome changed in {} cell(s). Review each difference, then rewrite \
         the grid with TYPED_MATRIX_BLESS=1.\n{}",
        difference.other.len(),
        difference.other.join("\n")
    );
    assert_eq!(
        std::fs::read_to_string(&path).ok(),
        Some(render_grid_source(&grid)),
        "{} is not what the generator writes (header or totals out of date); rewrite it with \
         TYPED_MATRIX_BLESS=1",
        path.display()
    );
}

/// The twelve programs that exercise an `int` field of
/// a user-defined type through `math.min`, `math.max`, `math.abs` and
/// `math.sign`, bound to an `int` and widened, for a per-bar object, a `var`
/// object and a field fed by a bounded input. The standalone backend must
/// accept each one, so they must be in the matrix and pinned accepted.
#[test]
fn the_twelve_int_field_cells_are_generated_and_pinned_accepted() {
    let generated: std::collections::BTreeSet<String> =
        cells().into_iter().map(|cell| cell.id).collect();
    let pinned: BTreeMap<String, String> = parse_grid(grid::PINNED).into_iter().collect();
    let mut checked = 0;
    for source in ["int.field", "int.field.var", "int.field.bounded"] {
        for consumer in [
            "math.min(v,3)",
            "math.max(v,3)",
            "math.abs(v)",
            "math.sign(v)",
        ] {
            let id = format!("{source}|{consumer}|widen");
            assert!(generated.contains(&id), "{id} is not generated");
            assert_eq!(
                pinned.get(&id).map(String::as_str),
                Some("ok"),
                "{id} must be pinned ok"
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 12);
}

fn front_end(source: &str) -> pinewasm_csil::CsilProgram {
    let result = compile(source, CompileOptions::all());
    assert!(
        !result
            .diagnostics
            .iter()
            .any(pinewasm_span::Diagnostic::is_error),
        "control source must compile: {:?}",
        result.diagnostics
    );
    result.program.expect("program")
}

/// The detector's self-test for internal refusals. A float stored into a slot
/// the program stores an int into is accepted by the validator and refused by
/// the backend with a type conflict; the front end cannot produce it, so it
/// is planted in compiled output (as in `refusal_classes.rs`). The matrix
/// must call that a defect, not a refusal it would pin.
#[test]
fn an_internal_refusal_is_a_defect_and_not_a_pinned_refusal() {
    use pinewasm_csil::{CsilInstr, F64Bits};
    let mut program = front_end(
        "//@version=6\nindicator(\"control\")\nvar int n = 0\nn := 5\nplot(close > open ? 1 : 0, \"Backtest Signal\")\n",
    );
    assert!(build(&program).is_ok(), "the unmodified self-test builds");
    let function = &mut program.functions[0];
    let slot = function
        .code
        .iter()
        .find_map(|instr| match instr {
            CsilInstr::StoreUserSlotCurrent { slot } => Some(*slot),
            _ => None,
        })
        .expect("the baseline stores its int slot");
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
    // The edit no longer matches the budget reference.
    function.budget_reference = None;
    match build(&program) {
        Err(Outcome::Defect(evidence)) => assert!(
            evidence.starts_with("internal refusal TypeConflict"),
            "{evidence}"
        ),
        other => panic!("expected a defect, got {other:?}"),
    }
}

/// The detector's self-test for wrong values: a module built from one program
/// compared with the interpreter running another must be a defect, and the
/// same module against its own program must be `ok`. The two programs differ
/// only in the "Value" lane, and only by na on the first bar.
#[test]
fn a_module_that_disagrees_with_the_interpreter_is_a_defect() {
    let data = synthetic_data(BARS);
    let wire = standalone_wasm_wire(&data, None);
    let template = |value: &str| {
        format!(
            "//@version=6\nindicator(\"control\")\n\
             plot(close > open ? 1 : 0, \"Backtest Signal\")\nplot({value}, \"Value\")\n"
        )
    };
    let current = front_end(&template("close"));
    let lagged = front_end(&template("close[1] * 0 + close"));
    let wasm = build(&current).expect("the self-test builds");
    assert_eq!(differ(&current, &wasm, &data, &wire), Outcome::Ok);
    match differ(&lagged, &wasm, &data, &wire) {
        Outcome::Defect(evidence) => assert!(
            evidence.starts_with("wrong value: plot 1 bar 0: interpreter Na"),
            "{evidence}"
        ),
        other => panic!("expected a defect, got {other:?}"),
    }
}

fn grid_of(cells: &[(&str, &str)]) -> Vec<(String, String)> {
    cells
        .iter()
        .map(|(id, value)| ((*id).to_owned(), (*value).to_owned()))
        .collect()
}

/// The comparison's own self-test: an accepted cell that becomes refused is an
/// acceptance regression and nothing else is.
#[test]
fn an_accepted_cell_that_is_refused_is_reported_as_an_acceptance_regression() {
    let pinned = grid_of(&[
        ("a", "ok"),
        ("b", "ok"),
        ("c", "UnsupportedIntegerNaEncoding"),
        ("d", "ok:runtime-error"),
        ("e", "UnsupportedIntegerNaEncoding"),
        ("gone", "ok"),
    ]);
    assert_eq!(compare_grids(&pinned, &pinned), GridDifference::default());

    let measured = grid_of(&[
        ("a", "UnsupportedIntegerNaEncoding"),
        ("b", "ok"),
        ("c", "ok"),
        ("d", "known-defect:example"),
        ("e", "UnsupportedOpcode"),
        ("new", "ok"),
    ]);
    let difference = compare_grids(&pinned, &measured);
    assert_eq!(
        difference.acceptance_regressions,
        [
            "a: was ok, now UnsupportedIntegerNaEncoding",
            "d: was ok:runtime-error, now known-defect:example",
        ]
    );
    assert_eq!(
        difference.other,
        [
            "c: was UnsupportedIntegerNaEncoding, now ok",
            "e: was UnsupportedIntegerNaEncoding, now UnsupportedOpcode",
            "new: not in the grid, now ok",
            "gone: was ok, no longer generated",
        ]
    );

    // A front-end rejection of a program that used to build is also a loss.
    let rejected = compare_grids(
        &grid_of(&[("a", "ok")]),
        &grid_of(&[("a", "frontend:E0001")]),
    );
    assert_eq!(
        rejected.acceptance_regressions,
        ["a: was ok, now frontend:E0001"]
    );
    assert!(rejected.other.is_empty());

    // The same cells in another order are a difference too: the file is the
    // generator's order.
    let reordered = compare_grids(
        &grid_of(&[("a", "ok"), ("b", "ok")]),
        &grid_of(&[("b", "ok"), ("a", "ok")]),
    );
    assert!(reordered.acceptance_regressions.is_empty());
    assert_eq!(reordered.other, ["the cell order changed"]);
}

/// The grid file round-trips: what is written is what is read back.
#[test]
fn the_grid_file_format_round_trips() {
    let grid = grid_of(&[
        ("int.field|math.min(v,3)|widen", "ok"),
        ("int.input|v+1|widen", "UnsupportedIntegerNaEncoding"),
        ("int.input|v/2|widen", "frontend:E0001"),
    ]);
    let text = render_grid(&grid);
    assert_eq!(parse_grid(&text), grid);
    assert!(text.contains("@total\tok\t1\n"), "{text}");
    assert!(text.contains("@total\tfrontend:E0001\t1\n"), "{text}");
}
