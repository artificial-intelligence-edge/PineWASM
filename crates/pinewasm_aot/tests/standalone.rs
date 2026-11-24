//! The standalone-emit gate: emit an artifact a foreign host can instantiate,
//! or refuse with the first unresolved dependency.
//!
//! Two things are being pinned here. First, that the gate accepts and refuses
//! the right programs. Second, and the reason the gate reads bytes rather
//! than `Layout` flags, that it would still refuse a module whose flags lie:
//! `audit_standalone` is checked directly against hand-built modules that no
//! emitter path produces.

use pinewasm_aot::{audit_standalone, compile_program, compile_standalone_program, AotError};
use pinewasm_compiler::{compile, CompileOptions};
use wasm_encoder::{
    EntityType, ExportKind, ExportSection, ImportSection, MemorySection, MemoryType, Module,
    TypeSection,
};

fn compile_csil(source: &str) -> pinewasm_csil::CsilProgram {
    let result = compile(source, CompileOptions::all());
    assert!(
        !result
            .diagnostics
            .iter()
            .any(pinewasm_span::Diagnostic::is_error),
        "sample must compile: {:?}",
        result.diagnostics
    );
    result.program.expect("program")
}

#[test]
fn a_program_needing_no_host_emits_a_standalone_artifact() {
    for (label, source) in [
        ("plain", "indicator(\"P\")\nplot(close)\n"),
        // Transcendentals link the numeric runtime into the module rather
        // than importing it, so they stay standalone.
        ("exp", "indicator(\"E\")\nplot(math.exp(close / 100))\n"),
        ("remainder", "indicator(\"R\")\nplot(close % 3.0)\n"),
        (
            "alertcondition",
            "indicator(\"A\")\nalertcondition(close > open, \"Up\", \"up bar\")\nplot(close)\n",
        ),
        // A `fill(...)` band's per-bar color is a lane store, so a banded
        // script asks a foreign host for nothing either.
        (
            "fill",
            "indicator(\"F\")\ntop = plot(high)\nbot = plot(low)\nfill(top, bot, color = close > open ? color.new(color.green, 50) : na)\n",
        ),
    ] {
        let module = compile_standalone_program(&compile_csil(source))
            .unwrap_or_else(|error| panic!("{label} should be standalone: {error}"));
        assert!(
            audit_standalone(&module.wasm).is_ok(),
            "{label}: emitted artifact must pass its own audit"
        );
    }
}

#[test]
fn a_ta_program_is_standalone_now_that_the_engine_is_linked() {
    let program = compile_csil("indicator(\"T\")\nplot(ta.ema(close, 9))\n");

    // A `ta.*` program is standalone now: the engine is inside the module, so
    // no `host.ta` import is needed and the gate passes it.
    let module = compile_program(&program).expect("native emit succeeds");
    assert!(module.layout.links_ta);
    assert!(
        audit_standalone(&module.wasm).is_ok(),
        "a ta artifact must pass its own audit: {:?}",
        audit_standalone(&module.wasm)
    );
    compile_standalone_program(&program).expect("the gate must pass a ta program");
}

#[test]
fn a_hosted_tier_program_is_refused() {
    // Strategies take the interpreter-hosted tier, which imports
    // `host.interpret` by construction. `compile_program` rejects them before
    // emission, so the gate reports the native blocker rather than an import.
    let program = compile_csil(
        "strategy(\"S\")\nif close > open\n    strategy.entry(\"L\", strategy.long)\nplot(close)\n",
    );
    let error = compile_standalone_program(&program).expect_err("gate must refuse it");
    assert!(
        matches!(error, AotError::StrategyRequiresInterpreter),
        "expected the native blocker to surface, got {error:?}"
    );
}

/// A module that imports something, with a properly bounded memory. No
/// emitter path produces this shape with `imports_ta` unset, which is the
/// point: the audit must catch an import the planner never recorded.
fn module_with_an_import() -> Vec<u8> {
    let mut types = TypeSection::new();
    types.ty().function([], []);
    let mut imports = ImportSection::new();
    imports.import("host", "ta", EntityType::Function(0));
    let mut memory = MemorySection::new();
    memory.memory(MemoryType {
        minimum: 1,
        maximum: Some(2),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    let mut module = Module::new();
    module.section(&types);
    module.section(&imports);
    module.section(&memory);
    module.finish()
}

/// A module importing nothing, whose memory may grow without bound.
fn module_with_unbounded_memory() -> Vec<u8> {
    let mut memory = MemorySection::new();
    memory.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    let mut module = Module::new();
    module.section(&memory);
    module.finish()
}

#[test]
fn the_audit_reads_bytes_rather_than_layout_flags() {
    let reason = audit_standalone(&module_with_an_import())
        .expect_err("an import must be refused whatever a flag says");
    assert!(
        reason.contains("host.ta"),
        "the refusal must name the import, got {reason:?}"
    );

    let reason = audit_standalone(&module_with_unbounded_memory())
        .expect_err("an unbounded memory must be refused");
    assert!(
        reason.contains("maximum"),
        "the refusal must name the missing bound, got {reason:?}"
    );
}

/// Decodes cleanly, and no engine will instantiate it: the export names
/// function index 0, which does not exist. Imports nothing, memory bounded,
/// so every structural check this audit makes passes.
fn module_that_decodes_but_does_not_validate() -> Vec<u8> {
    let mut memory = MemorySection::new();
    memory.memory(MemoryType {
        minimum: 1,
        maximum: Some(2),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    let mut exports = ExportSection::new();
    exports.export("run", ExportKind::Func, 0);

    let mut module = Module::new();
    module.section(&memory);
    module.section(&exports);
    module.finish()
}

#[test]
fn a_module_that_no_engine_would_instantiate_is_refused() {
    // The audit's claim is "a host can instantiate this", so decoding is not
    // enough: `Parser::parse_all` reads the binary format and checks none of
    // the semantic rules an engine applies.
    let bytes = module_that_decodes_but_does_not_validate();
    assert!(
        wasmparser::Parser::new(0)
            .parse_all(&bytes)
            .all(|payload| payload.is_ok()),
        "fixture must decode, or it is not testing what it claims to"
    );
    let reason =
        audit_standalone(&bytes).expect_err("a module that fails validation must be refused");
    assert!(
        reason.contains("not valid wasm"),
        "expected the validation failure to be named, got {reason:?}"
    );
}

#[test]
fn a_module_without_a_memory_is_refused() {
    // Not a shape we emit, but "no memory" must not read as "memory that
    // happens to satisfy every check we ran", which is what a loop over an
    // absent section does if nothing tracks that it ran at all.
    let empty = Module::new().finish();
    let reason = audit_standalone(&empty).expect_err("a module with no memory must be refused");
    assert!(
        reason.contains("no memory"),
        "expected the missing memory to be named, got {reason:?}"
    );
}

/// a literal `ReadUserSlot` offset sizes the slot ring, and nothing
/// used to bound it. `plan_layout` computes `(deepest + 1).next_power_of_two()`
/// and `emit` writes `slot_count * ring * 8` as an i32 — so a large enough
/// offset panicked the compiler in debug and silently emitted a module with a
/// wrapped, degenerate slot region in release.
///
/// The fix is layered, and this pins the layer that matters to a caller:
/// `preflight` validates, so the compiler *refuses* rather than reaching the
/// layout arithmetic at all. (`plan_layout` also clamps, which validation
/// makes unreachable — that is the point of it.)
#[test]
fn an_out_of_range_slot_offset_is_refused_not_compiled() {
    for offset in [1_000_000u32, 4_000_000_000, u32::MAX] {
        // A source with at least one series slot, so `slot: 0` is itself
        // in range and the offset is what the validator is left to judge.
        let mut program =
            compile_csil("//@version=6\nindicator(\"O\")\nx = close - close[1]\nplot(x)\n");
        program.functions[0].code.insert(
            0,
            pinewasm_csil::CsilInstr::ReadUserSlot { slot: 0, offset },
        );

        // The offset is what this pins; inserting an instruction also
        // desyncs the source map, so assert against the full error set
        // rather than whichever error `InvalidProgram` reports first.
        let errors = pinewasm_csil::validate(&program)
            .expect_err("an out-of-range offset must fail validation");
        assert!(
            errors.iter().any(|e| e.to_string().contains("offset")),
            "offset {offset} must be named among the errors, got {errors:?}"
        );

        // And the compiler must refuse rather than reach the layout
        // arithmetic — no panic, no artifact.
        assert!(
            matches!(compile_program(&program), Err(AotError::InvalidProgram(_))),
            "offset {offset} must be refused, not compiled"
        );
    }
}

#[test]
fn an_oversized_ta_engine_region_is_refused_rather_than_wrapped() {
    // Every ring capacity is a baked const param clamped to 100,000, but
    // nothing bounds the number of call sites, so the product is unbounded.
    // Enough of them would overflow the i32 the region size is emitted as,
    // which is a bug class this backend's layout arithmetic has produced
    // before: an unchecked layout multiply.
    // The refusal sends the program to the interpreter-hosted tier instead.
    let mut source = String::from("indicator(\"Big\")\ntotal = 0.0\n");
    for _ in 0..120 {
        source.push_str("total := total + math.sum(close, 100000)\n");
    }
    source.push_str("plot(total)\n");

    let program = compile_csil(&source);
    let error = compile_program(&program).expect_err("emission must refuse the region");
    let AotError::UnsupportedOpcode(reason) = &error else {
        panic!("expected UnsupportedOpcode, got {error:?}");
    };
    assert!(
        reason.contains("ta engine's state"),
        "the refusal must name what was too big, got {reason:?}"
    );
}

#[test]
fn a_realistic_ta_program_reserves_a_region_measured_in_kilobytes() {
    // The other half of the bound above: it exists to be checked, not
    // approached. A dense but ordinary script stays four orders of magnitude
    // under it.
    let program = compile_csil(
        "indicator(\"Dense\")\n\
         plot(ta.ema(close, 21))\n\
         plot(ta.rsi(close, 14))\n\
         plot(ta.stdev(close, 20))\n\
         plot(ta.wma(close, 50))\n\
         plot(ta.median(close, 33))\n\
         plot(ta.vwma(close, 20))\n\
         plot(ta.correlation(close, open, 30))\n",
    );
    let module = compile_program(&program).expect("native emit succeeds");
    assert!(module.layout.links_ta);
    let bytes = module.layout.ta_layout.region_bytes();
    assert!(
        bytes < 32 * 1024,
        "a seven-site script wanted {bytes} bytes of engine state"
    );
}
