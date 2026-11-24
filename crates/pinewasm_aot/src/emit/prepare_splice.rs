//! Test-only decoder check of the one step of the `emit_prepare` chain not
//! extracted to Lean: the splice of [`PrepareConsts`] and [`lane_offset`]
//! results into `emit_prepare`'s wasm-encoder instruction template.
//!
//! [`check`] decodes an emitted `prepare` body with `wasmparser` (not with
//! the encoder that wrote it), locates every template site by its
//! surrounding instruction pattern, and requires each `i32.const` operand to
//! equal the matching `PrepareConsts` field, the bar cap, or the proved lane
//! offset `lane * width`. Every emit in a test build runs it through
//! [`assert_splices`].

use super::*;
use wasmparser::{BinaryReader, FunctionBody, Operator};

/// The lane offsets `emit_prepare` should splice, computed from the plan
/// without [`lane_offset`]: the value the Lean proof gives it
/// (`lane * width`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExpectedLanes {
    builtin: Vec<i32>,
    plot: Vec<i32>,
    color: Vec<i32>,
}

impl ExpectedLanes {
    pub(super) fn of(layout: &Layout, globals: &Globals) -> Self {
        let mut builtin = Vec::new();
        for (lane, cell) in globals.builtin.iter().enumerate() {
            if cell.is_some() {
                builtin.push(i32::try_from(lane * 8).expect("small lane"));
            }
        }
        for j in 0..layout.source_inputs.len() + layout.security_lanes as usize {
            builtin.push(i32::try_from((BUILTIN_LANE_COUNT as usize + j) * 8).expect("small lane"));
        }
        let plot = (0..globals.plot.len())
            .map(|plot| i32::try_from(plot * 8).expect("small lane"))
            .collect();
        let color = (0..globals.color_lanes().count())
            .map(|lane| i32::try_from(lane * 4).expect("small lane"))
            .collect();
        Self {
            builtin,
            plot,
            color,
        }
    }
}

/// Everything [`check`] needs, recorded per emit so tests can re-check with
/// mutated inputs.
#[derive(Clone, Debug)]
pub(super) struct Splice {
    pub(super) body: Vec<u8>,
    pub(super) consts: PrepareConsts,
    pub(super) max_bars: u32,
    pub(super) lanes: ExpectedLanes,
}

thread_local! {
    /// Every `prepare` checked on this test thread.
    static CHECKED: std::cell::RefCell<Vec<Splice>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The splices [`assert_splices`] has checked on this thread.
pub(super) fn checked() -> Vec<Splice> {
    CHECKED.with(|checked| checked.borrow().clone())
}

/// Panic unless `prepare`'s decoded immediates are exactly `consts`,
/// `max_bars` and the plan's lane offsets. Called by every test-build `emit`.
pub(super) fn assert_splices(
    prepare: &Function,
    consts: &PrepareConsts,
    max_bars: u32,
    layout: &Layout,
    globals: &Globals,
) {
    use wasm_encoder::Encode;
    let mut body = Vec::new();
    prepare.encode(&mut body);
    let splice = Splice {
        body,
        consts: *consts,
        max_bars,
        lanes: ExpectedLanes::of(layout, globals),
    };
    if let Err(error) = check(&splice) {
        panic!("prepare splice mismatch: {error}");
    }
    CHECKED.with(|checked| checked.borrow_mut().push(splice));
}

/// Operator patterns, with `K` standing for the `i32.const` under test.
#[derive(Clone, Copy)]
enum P {
    K,
    Const(i32),
    Get(u32),
    Set(u32),
    GlobalSet,
    Mul,
    Add,
    And,
    GtU,
    Fill,
}

/// How one decoded operator relates to one pattern element.
enum Hit {
    Miss,
    Match,
    /// The `K` operand, with its value.
    Operand(i32),
}

fn matches(op: &Operator<'_>, p: P) -> Hit {
    let hit = match (op, p) {
        (Operator::I32Const { value }, P::K) => return Hit::Operand(*value),
        (Operator::I32Const { value }, P::Const(c)) => *value == c,
        (Operator::LocalGet { local_index }, P::Get(i)) => *local_index == i,
        (Operator::LocalSet { local_index }, P::Set(i)) => *local_index == i,
        (Operator::GlobalSet { .. }, P::GlobalSet) => true,
        (Operator::I32Mul, P::Mul) => true,
        (Operator::I32Add, P::Add) => true,
        (Operator::I32And, P::And) => true,
        (Operator::I32GtU, P::GtU) => true,
        (Operator::MemoryFill { mem: 0 }, P::Fill) => true,
        _ => false,
    };
    if hit {
        Hit::Match
    } else {
        Hit::Miss
    }
}

/// The `K` operand of every occurrence of `pattern`, in order.
fn find(ops: &[Operator<'_>], pattern: &[P]) -> Vec<i32> {
    let mut found = Vec::new();
    for window in ops.windows(pattern.len()) {
        let mut k = None;
        let mut ok = true;
        for (op, p) in window.iter().zip(pattern) {
            match matches(op, *p) {
                Hit::Operand(value) => k = Some(value),
                Hit::Match => {}
                Hit::Miss => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            found.push(k.expect("every pattern has one K"));
        }
    }
    found
}

fn once(ops: &[Operator<'_>], name: &str, pattern: &[P], expected: i32) -> Result<(), String> {
    let found = find(ops, pattern);
    if found == [expected] {
        Ok(())
    } else {
        Err(format!("{name}: expected [{expected}], decoded {found:?}"))
    }
}

/// Decode `splice.body` and compare every template immediate.
pub(super) fn check(splice: &Splice) -> Result<(), String> {
    use P::*;
    let mut reader = BinaryReader::new(&splice.body, 0);
    let size = reader.read_var_u32().map_err(|e| e.to_string())? as usize;
    let start = reader.current_position();
    let bytes = splice
        .body
        .get(start..start + size)
        .ok_or("truncated function body")?;
    let body = FunctionBody::new(BinaryReader::new(bytes, 0));
    let mut operators = body.get_operators_reader().map_err(|e| e.to_string())?;
    let mut ops = Vec::new();
    while !operators.eof() {
        ops.push(operators.read().map_err(|e| e.to_string())?);
    }

    // Locals: 0 = bars, 1 builtin, 2 slots, 3 written, 4 plots, 5 colors,
    // 6 scratch, 7 alerts, 8 ta, 9 heap_end (see `emit_prepare`).
    let c = &splice.consts;
    let max_bars = splice.max_bars as i32;
    once(&ops, "max_bars guard", &[Get(0), K, GtU], max_bars)?;
    once(&ops, "builtin_base", &[K, Set(1)], c.builtin_base)?;
    once(
        &ops,
        "builtin_stride",
        &[Get(1), Get(0), K, Mul, Add, Set(2)],
        c.builtin_stride,
    )?;
    once(&ops, "slot_bytes", &[Get(2), K, Add, Set(3)], c.slot_bytes)?;
    once(
        &ops,
        "written_align_addend",
        &[Get(3), K, Add, Const(!7), And, Set(4)],
        c.written_align_addend,
    )?;
    once(
        &ops,
        "plot_stride",
        &[Get(4), Get(0), K, Mul, Add, Set(5)],
        c.plot_stride,
    )?;
    once(
        &ops,
        "color_stride",
        &[
            Get(5),
            Get(0),
            K,
            Mul,
            Add,
            Const(7),
            Add,
            Const(!7),
            And,
            Set(6),
        ],
        c.color_stride,
    )?;
    once(
        &ops,
        "scratch_bytes",
        &[Get(6), K, Add, Set(7)],
        c.scratch_bytes,
    )?;
    once(
        &ops,
        "alert_stride",
        &[Get(7), Get(0), K, Mul, Add, Set(8)],
        c.alert_stride,
    )?;
    once(&ops, "ta_bytes", &[Get(8), K, Add, Set(9)], c.ta_bytes)?;
    once(
        &ops,
        "written_len",
        &[Get(3), Const(0), K, Fill],
        c.written_len,
    )?;

    for (name, base, expected) in [
        ("builtin lanes", 1, &splice.lanes.builtin),
        ("plot lanes", 4, &splice.lanes.plot),
        ("color lanes", 5, &splice.lanes.color),
    ] {
        let found = find(&ops, &[Get(base), Get(0), K, Mul, Add, GlobalSet]);
        if &found != expected {
            return Err(format!("{name}: expected {expected:?}, decoded {found:?}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{check, checked, Splice};
    use crate::{
        compile_program_to_wasm, compile_standalone_wasm_program, StandaloneWasmModuleType,
        StandaloneWasmOptions,
    };
    use pinewasm_compiler::{compile, CompileOptions};
    use std::path::{Path, PathBuf};

    fn collect_pine_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("readable fixture directory") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                collect_pine_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "pine") {
                out.push(path);
            }
        }
    }

    /// Programs that natively compile with a dynamic plot color, so the
    /// color-lane mutants always have a module to act on. The corpus alone
    /// does not guarantee one: its programs with dynamic colors can fall back
    /// to the VM tier (for example under the native `i64::MIN` admission gate).
    const LANE_SOURCES: &[&str] = &[
        "//@version=6\nindicator(\"lanes\")\nplot(close, color = close > open ? color.green : color.red)\n",
    ];

    /// Emit every repository program; return the splices checked on the way.
    fn corpus_splices() -> Vec<Splice> {
        let mut files = Vec::new();
        for dir in [
            "../../tests/compile-pass",
            "../../tests/regression",
            "../../benchmarks/workloads",
        ] {
            collect_pine_files(Path::new(dir), &mut files);
        }
        files.sort();
        let before = checked().len();
        let sources = files
            .iter()
            .map(|path| {
                std::fs::read_to_string(path)
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
            })
            .chain(LANE_SOURCES.iter().copied().map(String::from));
        for source in sources {
            let Some(program) = compile(&source, CompileOptions::default()).program else {
                continue;
            };
            let _ = compile_program_to_wasm(&program);
            for module_type in [
                StandaloneWasmModuleType::Strategy,
                StandaloneWasmModuleType::Indicator,
            ] {
                let _ = compile_standalone_wasm_program(
                    &program,
                    &StandaloneWasmOptions {
                        signal_plot: None,
                        module_type,
                    },
                );
            }
        }
        checked().split_off(before)
    }

    /// Every emitted `prepare` carries exactly its `PrepareConsts`, bar cap
    /// and lane offsets (checked inside `emit`), and the check is sharp:
    /// changing any single field, the bar cap, or one lane offset makes it
    /// fail on that module.
    #[test]
    fn decoded_prepare_immediates_are_the_prepare_consts() {
        let splices = corpus_splices();
        assert!(
            splices.len() >= 60,
            "only {} prepare functions checked",
            splices.len()
        );
        let mut lanes_seen = [0usize; 3];
        for splice in &splices {
            check(splice).expect("emitted splice");
            let mutants: [fn(&mut Splice); 11] = [
                |s| s.consts.builtin_base += 8,
                |s| s.consts.builtin_stride += 8,
                |s| s.consts.slot_bytes += 8,
                |s| s.consts.written_align_addend += 1,
                |s| s.consts.plot_stride += 8,
                |s| s.consts.color_stride += 4,
                |s| s.consts.scratch_bytes += 8,
                |s| s.consts.alert_stride += 8,
                |s| s.consts.ta_bytes += 8,
                |s| s.consts.written_len += 1,
                |s| s.max_bars ^= 1,
            ];
            for (index, mutate) in mutants.iter().enumerate() {
                let mut mutant = splice.clone();
                mutate(&mut mutant);
                assert!(check(&mutant).is_err(), "mutant {index} was not detected");
            }
            for (which, seen) in lanes_seen.iter_mut().enumerate() {
                let mut mutant = splice.clone();
                let lanes = match which {
                    0 => &mut mutant.lanes.builtin,
                    1 => &mut mutant.lanes.plot,
                    _ => &mut mutant.lanes.color,
                };
                if let Some(last) = lanes.last_mut() {
                    *last += 1;
                    *seen += 1;
                    assert!(
                        check(&mutant).is_err(),
                        "lane mutant {which} was not detected"
                    );
                }
            }
        }
        assert!(
            lanes_seen.iter().all(|&n| n > 0),
            "corpus lacks builtin, plot or color lanes: {lanes_seen:?}"
        );
        eprintln!(
            "[prepare-splice] checked={} with builtin/plot/color lanes={lanes_seen:?}",
            splices.len()
        );
    }

    /// The corpus has few color lanes; a program with styled plots and a
    /// styled fill (three color families) checks several color-lane offsets.
    #[test]
    fn color_lane_offsets_are_spliced() {
        let source = "//@version=6\nindicator(\"COLORS\")\n\
            top = plot(high, \"Top\", color = close > open ? color.green : color.red)\n\
            bot = plot(low, \"Bot\", color = close > open ? color.blue : color.orange)\n\
            fill(top, bot, color = close > open ? color.new(color.green, 60) : color.new(color.red, 60))\n";
        let program = compile(source, CompileOptions::default())
            .program
            .expect("fixture compiles");
        let before = checked().len();
        compile_program_to_wasm(&program).expect("fixture emits");
        let splices = checked().split_off(before);
        assert_eq!(splices.len(), 1);
        let splice = &splices[0];
        check(splice).expect("emitted splice");
        assert!(
            splice.lanes.color.len() >= 3,
            "expected three color lanes, got {:?}",
            splice.lanes.color
        );
        assert_eq!(
            splice.consts.color_stride,
            4 * splice.lanes.color.len() as i32
        );
        assert_eq!(splice.consts.plot_stride, 16);
        for lane in 0..splice.lanes.color.len() {
            let mut mutant = splice.clone();
            mutant.lanes.color[lane] += 4;
            assert!(
                check(&mutant).is_err(),
                "color lane {lane} mutant was not detected"
            );
        }
    }
}
