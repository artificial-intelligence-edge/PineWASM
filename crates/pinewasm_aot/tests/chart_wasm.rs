//! Linked chart modules (`compile_chart_wasm_program`, docs/aot.md §14)
//! against the interpreter.
//!
//! A linked chart module carries the VM as its own host, so it
//! imports nothing. Each case here instantiates the emitted module in `wasmi`
//! with an empty linker, drives it the way a loader does (`prepare`, the
//! lanes and numeric inputs, `chartInit`, `run`, `chartCommand`), and
//! requires what it returns to equal what `pinewasm_vm::run` returns for the
//! same program, data and options: every plot lane bit for bit, and every
//! color series, style, alert, log, drawing and strategy result as the same
//! wire value.
//!
//! The suite compiles only when the `pinewasm_chart_runtime` cfg is set.
#![cfg(pinewasm_chart_runtime)]

use pinewasm_aot::{
    chart_runtime_linked, compile_chart_wasm_program, compile_program, header, status, AotError,
    AotModule,
};
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_csil::CsilProgram;
use pinewasm_hir::InputValue;
use pinewasm_mir::Value;
use pinewasm_vm::{run, MarketData, RunOptions, RunOutcome};
use serde_json::{json, Value as Json};
use std::path::{Path, PathBuf};

/// `chartCommand` operations (`runtime/src/chart.rs`, `mod command`).
mod command {
    pub const TAKE_ERROR: i32 = 0;
    pub const ABI_MISMATCH: i32 = 1;
    pub const DRAIN_LOGS: i32 = 2;
    pub const DRAIN_ALERTS: i32 = 3;
    pub const DRAIN_STRATEGY: i32 = 4;
    pub const DRAIN_DRAWINGS: i32 = 5;
    pub const DISPLAY_STYLES: i32 = 6;
    pub const COLOR_SERIES: i32 = 7;
    pub const INIT_ERROR: i32 = 9;
}

fn compile_csil(source: &str) -> CsilProgram {
    let result = compile(source, CompileOptions::default());
    if let Some(d) = result.diagnostics.iter().find(|d| d.is_error()) {
        panic!("compile error: {:?} {}", d.code, d.message);
    }
    result.program.expect("compile must produce a program")
}

/// The differential suite's feed (`tests/support/mod.rs`).
fn synthetic_data(n: usize) -> MarketData {
    let mut data = MarketData::default();
    for i in 0..n {
        let base = 100.0 + (i as f64) * 0.05 + ((i as f64) * 0.7).sin() * 2.0;
        data.open.push(base);
        data.high.push(base + 1.0);
        data.low.push(base - 1.0);
        data.close.push(base + ((i as f64) * 1.3).cos() * 0.5);
        data.volume.push(1000.0 + (i % 97) as f64 * 10.0);
        data.time.push(1_000_000_000_000 + (i as i64) * 86_400_000);
    }
    data
}

fn u32_at(memory: &[u8], offset: u32) -> u32 {
    let at = offset as usize;
    u32::from_le_bytes(memory[at..at + 4].try_into().unwrap())
}

/// The names a module imports, read from its bytes.
fn imports_of(wasm: &[u8]) -> Vec<String> {
    let mut imports = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let wasmparser::Payload::ImportSection(reader) = payload.expect("module parses") {
            for group in reader {
                for entry in group.expect("import group") {
                    let (_, import) = entry.expect("import");
                    imports.push(format!("{}.{}", import.module, import.name));
                }
            }
        }
    }
    imports
}

/// A linked chart module as the engine holds it once it has parsed it: what
/// a host caches. An instance runs once, so a case that runs one module many
/// times instantiates it from here each time.
struct Loaded {
    engine: wasmi::Engine,
    parsed: wasmi::Module,
}

impl Loaded {
    fn new(module: &AotModule) -> Loaded {
        let engine = wasmi::Engine::default();
        let parsed = wasmi::Module::new(&engine, &module.wasm[..]).expect("wasmi accepts it");
        Loaded { engine, parsed }
    }
}

/// A linked chart module, instantiated with nothing to import.
struct Chart {
    store: wasmi::Store<()>,
    instance: wasmi::Instance,
    memory: wasmi::Memory,
}

impl Chart {
    fn new(module: &AotModule) -> Chart {
        Chart::instantiate(&Loaded::new(module))
    }

    fn instantiate(loaded: &Loaded) -> Chart {
        let (engine, parsed) = (&loaded.engine, &loaded.parsed);
        let mut store = wasmi::Store::new(engine, ());
        let instance = <wasmi::Linker<()>>::new(engine)
            .instantiate_and_start(&mut store, parsed)
            .expect("a linked chart module instantiates against an empty linker");
        let memory = instance
            .get_memory(&store, "memory")
            .expect("memory export");
        Chart {
            store,
            instance,
            memory,
        }
    }

    fn prepare(&mut self, bars: usize) -> i32 {
        self.instance
            .get_typed_func::<i32, i32>(&self.store, "prepare")
            .expect("prepare export")
            .call(&mut self.store, bars as i32)
            .expect("prepare does not trap")
    }

    fn run(&mut self) -> i32 {
        self.instance
            .get_typed_func::<(), i32>(&self.store, "run")
            .expect("run export")
            .call(&mut self.store, ())
            .expect("run does not trap")
    }

    fn alloc(&mut self, len: usize) -> u32 {
        self.instance
            .get_typed_func::<i32, i32>(&self.store, "chartAlloc")
            .expect("chartAlloc export")
            .call(&mut self.store, len as i32)
            .expect("chartAlloc does not trap") as u32
    }

    /// `chartInit` with `request`; the module's reason when it declines.
    fn init(&mut self, request: &Json) -> Result<(), String> {
        let bytes = serde_json::to_vec(request).unwrap();
        let at = self.alloc(bytes.len()) as usize;
        assert_ne!(at, 0, "the request allocation succeeds");
        self.memory.data_mut(&mut self.store)[at..at + bytes.len()].copy_from_slice(&bytes);
        let status = self
            .instance
            .get_typed_func::<(i32, i32), i32>(&self.store, "chartInit")
            .expect("chartInit export")
            .call(&mut self.store, (at as i32, bytes.len() as i32))
            .expect("chartInit does not trap");
        if status == status::OK {
            return Ok(());
        }
        assert_eq!(status, status::ABI_MISMATCH, "a declined request reroutes");
        match self.command(command::INIT_ERROR, 0, 0, 0) {
            Ok(Json::String(reason)) => Err(reason),
            other => panic!("a declined chartInit leaves its reason, got {other:?}"),
        }
    }

    /// The packed reply of `chartCommand`, before decoding.
    fn command_raw(&mut self, op: i32, a: i32, b: i32, c: i32) -> u64 {
        self.instance
            .get_typed_func::<(i32, i32, i32, i32), i64>(&self.store, "chartCommand")
            .expect("chartCommand export")
            .call(&mut self.store, (op, a, b, c))
            .expect("chartCommand does not trap") as u64
    }

    /// One `chartCommand`: its JSON reply, or the module's `chartError`.
    fn command(&mut self, op: i32, a: i32, b: i32, c: i32) -> Result<Json, String> {
        let packed = self.command_raw(op, a, b, c);
        let (at, len) = ((packed & 0xFFFF_FFFF) as usize, (packed >> 32) as usize);
        let memory = self.memory.data(&self.store);
        let reply: Json = serde_json::from_slice(&memory[at..at + len]).expect("a JSON reply");
        match reply.get("chartError") {
            Some(error) => Err(error.as_str().unwrap_or_default().to_string()),
            None => Ok(reply),
        }
    }

    fn memory(&self) -> &[u8] {
        self.memory.data(&self.store)
    }

    /// Write what a loader writes between `prepare` and `chartInit`: the
    /// builtin lanes, the numeric inputs and source lanes, and the budgets.
    /// String and color input cells are the linked host's to write, so they
    /// get a pattern no handle or color has.
    fn load(
        &mut self,
        module: &AotModule,
        program: &CsilProgram,
        data: &MarketData,
        options: &RunOptions,
        bars: usize,
    ) {
        let memory = self.memory.data_mut(&mut self.store);
        assert_eq!(u32_at(memory, header::OFF_MAGIC), header::MAGIC);
        assert_eq!(u32_at(memory, header::OFF_ABI_VERSION), header::ABI_VERSION);
        let builtin_base = u32_at(memory, header::OFF_BUILTIN_BASE) as usize;
        let norm = |v: f64| if v.is_finite() { v } else { f64::NAN };
        let lanes: [&[f64]; 5] = [&data.open, &data.high, &data.low, &data.close, &data.volume];
        for (lane, values) in lanes.iter().enumerate() {
            let base = builtin_base + lane * 8 * bars;
            for (bar, value) in values.iter().take(bars).enumerate() {
                memory[base + bar * 8..base + bar * 8 + 8]
                    .copy_from_slice(&norm(*value).to_le_bytes());
            }
        }
        let time_base = builtin_base + 5 * 8 * bars;
        for bar in 0..bars {
            let time = if data.time.is_empty() {
                1_577_836_800_000i64 + (bar as i64) * 86_400_000
            } else {
                data.time[bar]
            } as f64;
            memory[time_base + bar * 8..time_base + bar * 8 + 8]
                .copy_from_slice(&time.to_le_bytes());
        }
        for (index, decl) in program.inputs.iter().enumerate() {
            let chosen = options
                .inputs
                .get(&decl.id)
                .filter(|v| std::mem::discriminant(*v) == std::mem::discriminant(&decl.defval))
                .unwrap_or(&decl.defval);
            let cell = match chosen {
                InputValue::Int(v) => v.to_le_bytes(),
                InputValue::Float(v) => v.to_le_bytes(),
                InputValue::Bool(b) => f64::from(u8::from(*b)).to_le_bytes(),
                InputValue::Enum { index, .. } => i64::from(*index).to_le_bytes(),
                _ => 0x5A5A_5A5A_5A5A_5A5Au64.to_le_bytes(),
            };
            let at = header::SIZE as usize + index * 8;
            memory[at..at + 8].copy_from_slice(&cell);
        }
        for (lane, &input) in module.layout.source_inputs.iter().enumerate() {
            let decl = program
                .inputs
                .iter()
                .find(|decl| decl.id == input)
                .expect("source input declaration");
            let InputValue::Source(series) = &decl.defval else {
                panic!("source input {input} has a non-source default");
            };
            let base = builtin_base + (6 + lane) * 8 * bars;
            for bar in 0..bars {
                let value = norm(source_sample(data, *series, bar));
                memory[base + bar * 8..base + bar * 8 + 8].copy_from_slice(&value.to_le_bytes());
            }
        }
        let budget = header::OFF_BUDGET_PER_BAR as usize;
        memory[budget..budget + 8].copy_from_slice(&options.max_instructions_per_bar.to_le_bytes());
        let run_budget = u32_at(memory, header::OFF_SCRATCH_BASE) as usize
            + header::SCRATCH_OFF_BUDGET_PER_RUN as usize;
        memory[run_budget..run_budget + 8]
            .copy_from_slice(&options.max_instructions_per_run.to_le_bytes());
        let mintick = header::OFF_MINTICK as usize;
        memory[mintick..mintick + 8].copy_from_slice(&data.mintick.to_le_bytes());
        let elements = header::OFF_MAX_ELEMS as usize;
        memory[elements..elements + 8]
            .copy_from_slice(&(options.max_collection_elements as u64).to_le_bytes());
        let objects = header::OFF_MAX_OBJS as usize;
        memory[objects..objects + 8]
            .copy_from_slice(&pinewasm_aot::heap_defaults::MAX_OBJECTS.to_le_bytes());
    }
}

fn source_sample(data: &MarketData, series: pinewasm_hir::BuiltinSeries, bar: usize) -> f64 {
    use pinewasm_hir::BuiltinSeries as B;
    let (o, h, l, c) = (
        data.open[bar],
        data.high[bar],
        data.low[bar],
        data.close[bar],
    );
    match series {
        B::Open => o,
        B::High => h,
        B::Low => l,
        B::Close => c,
        B::Volume => data.volume[bar],
        B::Hl2 => h.midpoint(l),
        B::Hlc3 => (h + l + c) / 3.0,
        B::Ohlc4 => (o + h + l + c) / 4.0,
        B::Hlcc4 => (h + l + c + c) / 4.0,
        other => panic!("unsupported source input series {other:?}"),
    }
}

/// The request a loader passes to `chartInit`.
fn request(data: &MarketData, options: &RunOptions, bars: usize) -> Json {
    serde_json::from_str(&pinewasm_vm::chart_request_json(data, options, bars)).unwrap()
}

/// Everything a linked chart module returned for one run.
struct ChartRun {
    status: i32,
    err_bar: u32,
    err_pc: u32,
    plots: Vec<Vec<f64>>,
    /// One series per color lane, in lane order: plots, visuals, fills.
    colors: Vec<Json>,
    styles: Json,
    strategy: Json,
    alerts: Json,
    logs: Json,
    drawings: Json,
    error: Json,
    /// Why the host asked for the interpreter, when it did.
    mismatch: Json,
}

/// Run `module` over `data` as a loader does and read everything back.
fn run_chart(
    module: &AotModule,
    program: &CsilProgram,
    data: &MarketData,
    options: &RunOptions,
) -> ChartRun {
    run_loaded(&Loaded::new(module), module, program, data, options)
}

/// [`run_chart`] for a module the engine has already parsed.
fn run_loaded(
    loaded: &Loaded,
    module: &AotModule,
    program: &CsilProgram,
    data: &MarketData,
    options: &RunOptions,
) -> ChartRun {
    let bars = options
        .max_bars
        .map_or(data.bars(), |max| max.min(data.bars()));
    let mut chart = Chart::instantiate(loaded);
    assert_eq!(chart.prepare(bars), status::OK, "prepare");
    chart.load(module, program, data, options, bars);
    chart
        .init(&request(data, options, bars))
        .expect("the module accepts the request");
    let status = chart.run();
    let memory = chart.memory();
    assert_eq!(
        u32_at(memory, header::OFF_ERR_CODE) as i32,
        status,
        "run's return status and the header's agree"
    );
    let err_bar = u32_at(memory, header::OFF_ERR_BAR);
    let err_pc = u32_at(memory, header::OFF_ERR_PC);
    let plots_base = u32_at(memory, header::OFF_PLOTS_BASE) as usize;
    let plots = (0..module.layout.plot_count as usize)
        .map(|plot| {
            let base = plots_base + plot * 8 * bars;
            (0..bars)
                .map(|bar| {
                    f64::from_le_bytes(
                        memory[base + bar * 8..base + bar * 8 + 8]
                            .try_into()
                            .unwrap(),
                    )
                })
                .collect()
        })
        .collect();
    let visible = if status == status::OK {
        bars as u32
    } else {
        err_bar
    };
    let styles = chart
        .command(command::DISPLAY_STYLES, visible as i32, 0, 0)
        .expect("display styles");
    let mut colors = Vec::new();
    let families = [
        program
            .plots
            .iter()
            .map(|decl| decl.color_roles.len())
            .collect::<Vec<_>>(),
        program
            .visuals
            .iter()
            .map(|decl| decl.color_roles.len())
            .collect(),
        program
            .fills
            .iter()
            .map(|decl| decl.color_roles.len())
            .collect(),
    ];
    for (family, slots) in families.iter().enumerate() {
        for (slot, &roles) in slots.iter().enumerate() {
            for role in 0..roles {
                let reply = chart
                    .command(
                        command::COLOR_SERIES,
                        family as i32,
                        slot as i32,
                        role as i32,
                    )
                    .expect("color series");
                colors.push(expand_color_series(&reply));
            }
        }
    }
    // The strategy first: its order-fill alerts join the alert stream.
    let strategy = chart
        .command(command::DRAIN_STRATEGY, status, visible as i32, 0)
        .expect("strategy");
    let alerts = chart
        .command(command::DRAIN_ALERTS, 0, 0, 0)
        .expect("alerts");
    let logs = chart.command(command::DRAIN_LOGS, 0, 0, 0).expect("logs");
    let drawings = chart
        .command(command::DRAIN_DRAWINGS, 0, 0, 0)
        .expect("drawings");
    let error = chart.command(command::TAKE_ERROR, 0, 0, 0).expect("error");
    let mismatch = chart
        .command(command::ABI_MISMATCH, 0, 0, 0)
        .expect("mismatch");
    ChartRun {
        status,
        err_bar,
        err_pc,
        plots,
        colors,
        styles,
        strategy,
        alerts,
        logs,
        drawings,
        error,
        mismatch,
    }
}

/// A color lane's reply (`{series, index}`: the series of its distinct
/// colors, and which of them each bar shows) as the per-bar series.
fn expand_color_series(reply: &Json) -> Json {
    let series = &reply["series"];
    let index = reply["index"].as_array().expect("index");
    let pick = |field: &str| -> Vec<Json> {
        let entries = series[field].as_array().expect("series field");
        index
            .iter()
            .map(|entry| entries[entry.as_u64().expect("index entry") as usize].clone())
            .collect()
    };
    json!({
        "role": series["role"],
        "values": pick("values"),
        "paletteIndices": pick("paletteIndices"),
    })
}

/// An interpreter result as its wire value. A macro, not a function: the
/// crate's tests name `serde_json` but not `serde` itself.
macro_rules! wire {
    ($value:expr) => {
        serde_json::to_value($value).expect("wire value")
    };
}

/// The first difference between what a linked run returned and the
/// interpreter's results over the `bars` bars both completed: every plot
/// lane bit for bit, and the color series, styles, strategy results, alerts,
/// logs and drawings as wire values.
fn results_differ(reference: &RunOutcome, chart: &ChartRun, bars: usize) -> Option<String> {
    if reference.plots.len() != chart.plots.len() {
        return Some(format!(
            "plots: interpreter {}, module {}",
            reference.plots.len(),
            chart.plots.len()
        ));
    }
    for (plot, lane) in reference.plots.iter().zip(&chart.plots) {
        if plot.values.len() != bars {
            return Some(format!(
                "plot {}: the interpreter holds {} bars of {bars}",
                plot.slot,
                plot.values.len()
            ));
        }
        for (bar, (expected, got)) in plot.values.iter().zip(lane).enumerate() {
            let same = match expected {
                Value::Na => got.is_nan(),
                Value::Float(x) => x.to_bits() == got.to_bits(),
                _ => false,
            };
            if !same {
                return Some(format!(
                    "plot {} bar {bar}: interpreter {expected:?}, module {got}",
                    plot.slot
                ));
            }
        }
    }
    let expected_colors: Vec<Json> = reference
        .plots
        .iter()
        .flat_map(|plot| plot.colors.iter())
        .chain(
            reference
                .visuals
                .iter()
                .flat_map(|visual| visual.colors.iter()),
        )
        .chain(reference.fills.iter().flat_map(|fill| fill.colors.iter()))
        .map(|series| wire!(series))
        .collect();
    if expected_colors.len() != chart.colors.len() {
        return Some(format!(
            "color lanes: interpreter {}, module {}",
            expected_colors.len(),
            chart.colors.len()
        ));
    }
    for (lane, (expected, got)) in expected_colors.iter().zip(&chart.colors).enumerate() {
        if expected != got {
            return Some(format!(
                "color lane {lane}: interpreter {expected}, module {got}"
            ));
        }
    }
    let expected_styles = wire!(&pinewasm_vm::NativeDisplayStyles {
        plots: reference
            .plots
            .iter()
            .map(|plot| plot.style.clone())
            .collect(),
        visuals: reference
            .visuals
            .iter()
            .map(|visual| visual.style.clone())
            .collect(),
        fills: reference
            .fills
            .iter()
            .map(|fill| fill.style.clone())
            .collect(),
    });
    [
        ("display styles", expected_styles, &chart.styles),
        ("strategy", wire!(&reference.strategy), &chart.strategy),
        ("alerts", wire!(&reference.alerts), &chart.alerts),
        ("logs", wire!(&reference.logs), &chart.logs),
        ("drawings", wire!(&reference.drawings), &chart.drawings),
    ]
    .into_iter()
    .find(|(_, expected, got)| expected != *got)
    .map(|(what, expected, got)| format!("{what}: interpreter {expected}, module {got}"))
}

/// Require a linked chart module's run to equal the interpreter's.
fn assert_run_matches(reference: &RunOutcome, chart: &ChartRun, bars: usize, context: &str) {
    assert!(
        reference.errors.is_empty(),
        "{context}: the interpreter run fails: {:?}",
        reference.errors
    );
    assert_eq!(
        chart.status,
        status::OK,
        "{context}: status (bar {}, host: {})",
        chart.err_bar,
        chart.mismatch
    );
    assert_eq!(chart.error, Json::Null, "{context}: no host error");
    if let Some(difference) = results_differ(reference, chart, bars) {
        panic!("{context}: {difference}");
    }
}

/// How a linked run stands to the interpreter's run of the same request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Agreement {
    /// Both finished, with the same results.
    Finished,
    /// Both stopped: on the same bar, at the same instruction, with the same
    /// error, and with the same results over the bars before it.
    Stopped,
    /// The interpreter stopped and the module handed the run back, for the
    /// interpreter to produce that stop.
    HandedBack,
}

/// How `chart` stands to the interpreter's run, or why it stands in none of the three
/// ways a linked run may: a run the interpreter finishes must finish, with
/// its results, and a run the interpreter stops must stop exactly as it does
/// or be handed back.
fn agreement(reference: &RunOutcome, chart: &ChartRun) -> Result<Agreement, String> {
    let bars = reference.bars_executed as usize;
    let Some(error) = reference.errors.first() else {
        if chart.status != status::OK || chart.error != Json::Null {
            return Err(format!(
                "the interpreter finishes; the module returns status {} at bar {} (error {}, host: {})",
                chart.status, chart.err_bar, chart.error, chart.mismatch
            ));
        }
        return results_differ(reference, chart, bars).map_or(Ok(Agreement::Finished), Err);
    };
    if matches!(
        chart.status,
        status::ABI_MISMATCH | status::HEAP_REROUTE | status::RUN_BUDGET_REROUTE
    ) {
        return Ok(Agreement::HandedBack);
    }
    // A stop of its own is the linked host's: status 5 and its error.
    if chart.status != pinewasm_vm::aot_host::vm_status::HOST_ERROR
        || chart.error != wire!(error)
        || Some(chart.err_bar) != error.bar_index
        || Some(chart.err_pc) != error.instruction_offset
    {
        return Err(format!(
            "the interpreter stops with {error:?}; the module returns status {} at bar {} pc {} with {}",
            chart.status, chart.err_bar, chart.err_pc, chart.error
        ));
    }
    results_differ(reference, chart, bars).map_or(Ok(Agreement::Stopped), Err)
}

/// [`agreement`], required.
fn assert_agrees(reference: &RunOutcome, chart: &ChartRun, context: &str) -> Agreement {
    agreement(reference, chart).unwrap_or_else(|why| panic!("{context}: {why}"))
}

/// Compile `source` as a linked chart module and require it to match the
/// interpreter over each of `bar_counts` bars. Returns the module.
fn assert_source_matches(source: &str, options: &RunOptions, bar_counts: &[usize]) -> AotModule {
    let program = compile_csil(source);
    let module = compile_chart_wasm_program(&program)
        .unwrap_or_else(|error| panic!("linked chart compile: {error}\n{source}"));
    assert_eq!(imports_of(&module.wasm), Vec::<String>::new(), "{source}");
    pinewasm_aot::audit_standalone(&module.wasm).expect("the import-free audit accepts it");
    for &bars in bar_counts {
        let data = synthetic_data(bars);
        let reference = run(&program, &data, options);
        let chart = run_chart(&module, &program, &data, options);
        assert_run_matches(
            &reference,
            &chart,
            bars,
            &format!("{bars} bars of\n{source}"),
        );
    }
    module
}

#[test]
fn primary_non_finite_samples_reach_the_linked_host_without_rerouting() {
    let program = compile_csil(
        "//@version=6\nindicator(\"Primary samples\")\nlabel.new(bar_index, close, str.tostring(volume))\nplot(nz(open))\nplot(nz(high))\nplot(nz(low))\nplot(nz(close))\nplot(nz(volume))\n",
    );
    let module = compile_chart_wasm_program(&program).expect("compiles");
    let mut data = synthetic_data(6);
    let samples = vec![f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0, 0.0, 1.25];
    data.open = samples.clone();
    data.high = samples.clone();
    data.low = samples.clone();
    data.close = samples.clone();
    data.volume = samples;
    let options = RunOptions::default();
    let reference = run(&program, &data, &options);
    let chart = run_chart(&module, &program, &data, &options);
    assert_run_matches(
        &reference,
        &chart,
        data.bars(),
        "non-finite primary samples",
    );
}

#[test]
fn this_compiler_links_the_chart_runtime() {
    assert!(chart_runtime_linked());
}

/// The module's whole surface: nothing imported, and exactly the exports a
/// loader uses.
#[test]
fn a_linked_chart_module_imports_nothing_and_exports_its_loader_surface() {
    let program = compile_csil(
        "//@version=6\nindicator(\"Surface\")\nlabel.new(bar_index, close, str.tostring(close))\nplot(close)\n",
    );
    let module = compile_chart_wasm_program(&program).expect("compiles");
    assert_eq!(imports_of(&module.wasm), Vec::<String>::new());
    pinewasm_aot::audit_standalone(&module.wasm).expect("the import-free audit accepts it");
    assert!(!module.layout.imports_vm && !module.layout.imports_interpreter);
    assert!(module.layout.links_runtime);
    assert!(
        !module.layout.host_sites.is_empty(),
        "the label is a host site"
    );
    let mut exports = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(&module.wasm) {
        if let wasmparser::Payload::ExportSection(reader) = payload.unwrap() {
            for export in reader {
                exports.push(export.unwrap().name.to_string());
            }
        }
    }
    exports.sort();
    assert_eq!(
        exports,
        [
            "chartAlloc",
            "chartCommand",
            "chartInit",
            "memory",
            "prepare",
            "run"
        ]
    );
    // The same program, compiled for an imported host, imports `host.vm`.
    let imported = compile_program(&program).expect("compiles");
    assert_eq!(imports_of(&imported.wasm), ["host.vm"]);
}

/// Every instruction class a chart module leaves to its host, run by the
/// linked one: text, drawings, tables, logs, alerts and requests.
#[test]
fn hosted_instructions_match_the_interpreter() {
    let options = RunOptions::default();
    for body in [
        "label.new(bar_index, high, str.tostring(close, \"#.00\") + \" @ \" + str.tostring(bar_index))\nplot(close)\n",
        "var line l = na\nif bar_index % 5 == 0\n    l := line.new(bar_index - 1, low, bar_index, high, color=color.red, width=2)\nplot(na(l) ? 0 : 1)\n",
        "var box b = box.new(0, 110, 3, 90, text=\"zone\")\nif bar_index % 7 == 0\n    box.set_right(b, bar_index)\nplot(box.get_right(b))\n",
        // Getters on an `na` id read `na` until the drawing exists, then its
        // fields, natively and in the interpreter alike.
        "var line l = na\nvar label lb = na\nvar box bx = na\nvar linefill lf = na\nif bar_index == 6\n    l := line.new(0, low, bar_index, high)\n    lb := label.new(bar_index, high, \"t\")\nplot(line.get_x1(l))\nplot(line.get_price(l, bar_index))\nplot(label.get_y(lb))\nplot(box.get_top(bx))\nplot(na(linefill.get_line1(lf)) ? 1 : 0)\nplot(line.get_y2(na))\n",
        "var table t = table.new(position.top_right, 2, 2)\nif barstate.islast\n    table.cell(t, 0, 0, \"close\")\n    table.cell(t, 1, 0, str.tostring(close))\nplot(close)\n",
        "if bar_index % 4 == 0\n    log.info(\"bar {0} close {1}\", bar_index, close)\nif bar_index % 9 == 0\n    log.warning(\"nine\")\nplot(close)\n",
        "if ta.crossover(close, ta.sma(close, 5))\n    alert(\"up \" + str.tostring(close), alert.freq_once_per_bar)\nalertcondition(close > open, \"Green\", \"green bar\")\nplot(close)\n",
        "s = syminfo.ticker + \":\" + timeframe.period\nplot(str.length(s))\n",
        "a = array.new_string(0)\narray.push(a, str.tostring(bar_index % 3))\narray.push(a, \"x\")\nplot(array.size(a) + (array.get(a, 0) == \"1\" ? 10 : 0))\n",
        "m = map.new<string, float>()\nmap.put(m, \"c\", close)\nmap.put(m, \"o\", open)\nplot(map.get(m, \"c\") - map.get(m, \"o\"))\n",
        "plot(request.security(syminfo.tickerid, \"D\", close))\n",
    ] {
        let source = format!("//@version=6\nindicator(\"Hosted\", overlay=true)\n{body}");
        assert_source_matches(&source, &options, &[0, 1, 37]);
    }
}

#[test]
fn table_delete_aliases_refuse_native_and_run_through_run_program() {
    let source = r#"//@version=6
indicator("Deleted table aliases")
var table t = table.new(position.top_left, 1, 1)
var table copied = t
var array<table> ids = array.new_table()
if barstate.isfirst
    array.push(ids, t)
    table.cell(t, 0, 0, "before delete")
    table.delete(t)
plot(na(t) ? 1 : 0, "Original")
plot(na(copied) ? 1 : 0, "Copied alias")
plot(na(array.get(ids, 0)) ? 1 : 0, "Array alias")
"#;
    let program = compile_csil(source);
    let refusal = compile_chart_wasm_program(&program)
        .expect_err("native slots cannot be invalidated by the hosted table delete");
    assert_eq!(
        refusal,
        AotError::UnsupportedOpcode("table.delete alias invalidation requires interpreter".into())
    );

    // `runProgram` is the cached-program interpreter entry point. A loader
    // that receives the AOT refusal can use this path for the whole script.
    let data = synthetic_data(2);
    let result = pinewasm_compiler::run_program(&program, &data, &RunOptions::default());
    assert!(
        result.runtime_errors.is_empty(),
        "interpreter errors: {:?}",
        result.runtime_errors
    );
    for plot in &result.plots {
        assert_eq!(plot.values, vec![Value::Float(1.0); 2]);
    }
    assert!(result.drawings.tables.is_empty());
}

#[test]
fn same_position_table_replacement_refuses_aot_and_preserves_aliases_in_interpreter() {
    let source = r#"//@version=6
indicator("Table position replacement")
var table first = table.new(position.top_left, 1, 1)
var table alias = first
var array<table> ids = array.new_table()
array.push(ids, first)
if barstate.isfirst
    table.cell(first, 0, 0, "old")
table replacement = table.new(position.top_left, 2, 1, force_overlay = true)
table.cell(replacement, 1, 0, "new")
plot(na(first) ? 1 : 0, "Original")
plot(na(alias) ? 1 : 0, "Copied alias")
plot(na(array.get(ids, 0)) ? 1 : 0, "Array alias")
plot(array.size(table.all), "Live tables")
"#;
    let program = compile_csil(source);
    let refusal =
        AotError::UnsupportedOpcode("table position replacement requires interpreter".into());
    assert_eq!(
        compile_chart_wasm_program(&program).unwrap_err(),
        refusal,
        "the linked native module cannot rewrite its table aliases"
    );

    let data = synthetic_data(2);
    let result = pinewasm_compiler::run_program(&program, &data, &RunOptions::default());
    assert!(
        result.runtime_errors.is_empty(),
        "interpreter errors: {:?}",
        result.runtime_errors
    );
    assert_eq!(result.drawings.tables.len(), 1);
    assert_eq!(
        result.drawings.tables[0].fields.get("columns"),
        Some(&Value::Int(2))
    );
    assert_eq!(result.drawings.tables[0].cells.len(), 1);
    assert_eq!(
        result.drawings.tables[0].cells[0].attrs.get("text"),
        Some(&Value::String("new".into()))
    );
    for plot in &result.plots[..3] {
        assert_eq!(plot.values, vec![Value::Float(1.0); 2]);
    }
    assert_eq!(result.plots[3].values, vec![Value::Float(1.0); 2]);
}

/// A linked host gives equal strings one handle, so the module compares
/// handles itself. The comparison has to agree with the interpreter's for
/// constants, inputs, strings a hosted instruction built, and `na`.
#[test]
fn string_comparisons_are_native_and_match_the_interpreter() {
    let source = "//@version=6\nindicator(\"Strings\")\n\
mode = input.string(\"fast\", options=[\"fast\", \"slow\", \"off\"])\n\
label_text = input.string(\"free text\")\n\
built = str.tostring(bar_index % 3)\n\
string missing = na\n\
maybe = bar_index % 2 == 0 ? mode : missing\n\
plot(mode == \"fast\" ? 1 : mode == \"slow\" ? 2 : 3)\n\
plot(mode != \"off\" ? 1 : 0)\n\
plot(built == \"1\" ? 1 : 0)\n\
plot(built != \"2\" ? 1 : 0)\n\
plot(built + \"x\" == \"0x\" ? 1 : 0)\n\
plot(label_text == \"free text\" ? 1 : 0)\n\
plot(label_text == mode ? 1 : 0)\n\
plot(maybe == \"fast\" ? 1 : 0)\n\
plot(maybe != \"fast\" ? 1 : 0)\n\
plot(missing == missing ? 1 : 0)\n\
plot(str.upper(mode) == \"FAST\" ? 1 : 0)\n";
    let program = compile_csil(source);
    let module = compile_chart_wasm_program(&program).expect("compiles");
    // No comparison and no input read is a host site: only the instructions
    // that build a string are.
    let code = &program.main().code;
    for site in &module.layout.host_sites {
        let instruction = format!("{:?}", code[site.pc as usize]);
        assert!(
            !instruction.starts_with("CmpEq")
                && !instruction.starts_with("CmpNe")
                && !instruction.starts_with("ReadInput"),
            "{instruction} is a host site"
        );
    }
    let mode = program
        .inputs
        .iter()
        .find(|decl| decl.title.as_deref() == Some("mode"))
        .or_else(|| program.inputs.first())
        .expect("mode input")
        .id;
    let text = program.inputs[1].id;
    for (mode_value, text_value) in [
        (None, None),
        (Some("slow"), None),
        (Some("off"), Some("off")),
        (Some("fast"), Some("fast")),
        (Some("fast"), Some("1")),
    ] {
        let mut options = RunOptions::default();
        if let Some(value) = mode_value {
            options
                .inputs
                .insert(mode, InputValue::String(value.to_string()));
        }
        if let Some(value) = text_value {
            options
                .inputs
                .insert(text, InputValue::String(value.to_string()));
        }
        let data = synthetic_data(13);
        let reference = run(&program, &data, &options);
        let chart = run_chart(&module, &program, &data, &options);
        assert_run_matches(
            &reference,
            &chart,
            13,
            &format!("mode={mode_value:?} text={text_value:?}"),
        );
    }
}

/// A program whose constant pool repeats a text cannot have its handles
/// compared, so its string comparisons stay with the host. The compiler's
/// own pool never repeats one; a hand-built program can.
#[test]
fn a_repeated_string_constant_keeps_comparisons_on_the_host() {
    let mut program = compile_csil(
        "//@version=6\nindicator(\"Pool\")\na = bar_index % 2 == 0 ? \"x\" : \"y\"\nplot(a == \"x\" ? 1 : 0)\n",
    );
    let repeated = program.constants[0].clone();
    program.constants.push(repeated);
    let module = compile_chart_wasm_program(&program).expect("compiles");
    let code = &program.main().code;
    assert!(
        module
            .layout
            .host_sites
            .iter()
            .any(|site| format!("{:?}", code[site.pc as usize]).starts_with("CmpEq")),
        "the comparison is a host site"
    );
    let data = synthetic_data(9);
    let options = RunOptions::default();
    let reference = run(&program, &data, &options);
    let chart = run_chart(&module, &program, &data, &options);
    assert_run_matches(&reference, &chart, 9, "repeated constant");
}

/// A module with no host site records its `alertcondition`s in its own alert
/// lane and runs none of them on its host. The alert command returns that
/// lane, so a loader reads alerts one way whichever kind of module it
/// loaded.
#[test]
fn a_module_without_host_sites_returns_its_own_alerts() {
    let source = "//@version=6\nindicator(\"Lane\")\nalertcondition(close > open, \"Up\", \"closed up\")\nalertcondition(close < open, \"Down\")\nplot(close)\n";
    let program = compile_csil(source);
    let module = compile_chart_wasm_program(&program).expect("compiles");
    assert!(module.layout.host_sites.is_empty(), "no host site");
    assert_eq!(module.layout.alert_sites, 2, "both write the native lane");
    let data = synthetic_data(40);
    let options = RunOptions::default();
    let reference = run(&program, &data, &options);
    assert!(
        reference.alerts.len() > 10,
        "the feed fires both conditions: {}",
        reference.alerts.len()
    );
    assert!(reference
        .alerts
        .iter()
        .any(|alert| alert.message.is_empty()));
    let chart = run_chart(&module, &program, &data, &options);
    assert_run_matches(&reference, &chart, 40, "native alert lane");
}

/// What a packed native color cell cannot hold: a fractional transparency,
/// and which color input a value came from. `compile_program` refuses these
/// programs; a linked host keeps the full colors.
#[test]
fn full_colors_match_the_interpreter() {
    for (body, refusal) in [
        ("c = #12345680\nplot(close, color=c)\nplot(color.t(c))\n", "fractional color transparency"),
        ("c = color.rgb(18, 52, 86, 47.6)\nplot(close, color=c)\nplot(color.t(c))\n", "fractional color transparency"),
        ("c = color.new(#123456, input.float(47.6))\nplot(close, color=c)\nplot(color.t(c))\n", "unproven color transparency"),
        ("c = color.from_gradient(close, 99, 103, color.new(#123456, 20), color.new(#fedcba, 55))\nplot(close, color=c)\nplot(color.r(c))\nplot(color.t(c))\nbgcolor(c)\n", "unproven color transparency"),
        ("up = input.color(#00ff00, \"Up\")\ndown = input.color(color.new(#ff0000, 30), \"Down\")\nc = close > open ? up : down\nplot(close, color=c)\nplot(color.g(c) + color.t(c))\nplot(c == up ? 1 : 0)\nbarcolor(color.new(c, 80))\n", "color input provenance"),
        ("a = input.color(#112233)\nb = input.color(#112233)\nplot(close, color=bar_index % 2 == 0 ? a : b)\nplot(a == b ? 1 : 0)\n", "color input provenance"),
        ("p1 = plot(high)\np2 = plot(low)\nfill(p1, p2, color=color.new(input.color(#3366ff), 85.5))\n", "unproven color transparency"),
    ] {
        let source = format!("//@version=6\nindicator(\"Colors\", overlay=true)\n{body}");
        let program = compile_csil(&source);
        assert_eq!(
            compile_program(&program).unwrap_err(),
            AotError::UnsupportedOpcode(refusal.into()),
            "{body}"
        );
        assert_source_matches(&source, &RunOptions::default(), &[0, 1, 33]);
    }
}

/// A color input's override reaches the module through the cell its linked
/// host writes, with the input's identity.
#[test]
fn color_input_overrides_match_the_interpreter() {
    let source = "//@version=6\nindicator(\"Color input\")\nc = input.color(#123456)\nplot(close, color=c)\nplot(color.r(c) + color.t(c))\n";
    let program = compile_csil(source);
    let module = compile_chart_wasm_program(&program).expect("compiles");
    for transparency in [0.0, 30.0, 47.6] {
        let mut options = RunOptions::default();
        options.inputs.insert(
            program.inputs[0].id,
            InputValue::Color {
                r: 200,
                g: 100,
                b: 50,
                t: transparency,
            },
        );
        let data = synthetic_data(11);
        let reference = run(&program, &data, &options);
        let chart = run_chart(&module, &program, &data, &options);
        assert_run_matches(
            &reference,
            &chart,
            11,
            &format!("transparency {transparency}"),
        );
    }
}

/// A fixed presentation value computed by the script (a line width from an
/// input) is read by the linked host from a slot the program itself never
/// reads back. A linked module writes the per-bar cells only of slots
/// something reads, so a style's slot must stay among them.
#[test]
fn display_styles_resolved_from_slots_match_the_interpreter() {
    let mut from_slots = 0;
    for body in [
        "n = input.int(7)\nplot(close, linewidth=int(math.pow(n, 2)))\n",
        "n = input.int(2)\nx = close * 2.0\ny = x + 1.0\nplot(y, linewidth=int(math.pow(n, 2)))\nplot(x[1], linewidth=int(math.max(n, 3)))\n",
        "n = input.int(3)\nvar float held = na\nheld := nz(held) + close\nplot(held, linewidth=int(math.pow(n, 2)))\nif bar_index % 3 == 0\n    label.new(bar_index, high, str.tostring(held))\n",
        "n = input.int(2)\nw = n + 1\nplot(close, linewidth=w)\n",
    ] {
        let source = format!("//@version=6\nindicator(\"Styles\")\n{body}");
        for bars in [0, 1, 2, 29] {
            assert_source_matches(&source, &RunOptions::default(), &[bars]);
        }
        let program = compile_csil(&source);
        from_slots += usize::from(
            program
                .plots
                .iter()
                .any(|plot| !plot.style.runtime_scalars.is_empty()),
        );
    }
    assert!(
        from_slots >= 3,
        "the fixtures must resolve styles from slots, {from_slots} do"
    );
}

/// A color call of a program with full colors is a host site, and the module
/// calls its host there only for an operand or a result a packed cell
/// cannot hold. Each body below mixes the two bar by bar: whole and
/// fractional transparencies, both zeros, `na`, values outside 0..=100,
/// integer and float operands, color inputs and constants.
#[test]
fn color_calls_match_the_interpreter_packed_or_full_bar_by_bar() {
    // The input keeps every one of these programs out of the packed tier.
    let prelude = "tint = input.color(color.new(#3366ff, 30), \"Tint\")\n";
    for body in [
        // color.new: a float transparency of every kind.
        "t = bar_index % 6 == 0 ? 47.6 : bar_index % 6 == 1 ? 0.0 : bar_index % 6 == 2 ? 100.0 : bar_index % 6 == 3 ? 150.0 : bar_index % 6 == 4 ? -5.0 : 20.0\nc = color.new(color.red, t)\nplot(close, color=c)\nplot(color.t(c))\nplot(color.r(c) + color.g(c) + color.b(c))\n",
        "zero = bar_index % 2 == 0 ? 0.0 : 0.0 * -1.0\nc = color.new(#102030, zero)\nplot(close, color=c)\nplot(color.t(c))\n",
        "float t = bar_index % 3 == 0 ? na : bar_index % 3 == 1 ? 12.0 : 12.5\nc = color.new(color.blue, t)\nplot(close, color=c)\nplot(na(c) ? -1 : color.t(c))\n",
        // color.new: an integer transparency of any size, and na.
        "int t = bar_index % 5 == 0 ? na : bar_index * 37 - 60\nc = color.new(color.green, t)\nplot(close, color=c)\nplot(na(c) ? -1 : color.t(c))\n",
        // color.new: the color is packed, the input's handle, or na.
        "base = bar_index % 3 == 0 ? tint : bar_index % 3 == 1 ? color.new(color.orange, 40) : color(na)\nc = color.new(base, bar_index % 2 == 0 ? 10 : 55.5)\nplot(close, color=c)\nplot(na(c) ? -1 : color.r(c) + color.t(c))\n",
        // Channel reads of a packed color, a handle, a fractional color, na.
        "c = bar_index % 4 == 0 ? tint : bar_index % 4 == 1 ? color.rgb(1, 2, 3, 4) : bar_index % 4 == 2 ? color.new(color.red, 33.3) : color(na)\nplot(na(c) ? -1 : color.r(c))\nplot(na(c) ? -1 : color.g(c))\nplot(na(c) ? -1 : color.b(c))\nplot(na(c) ? -1 : color.t(c))\nplot(close, color=c)\n",
        // color.rgb: float channels (fractions, out of range, na), every
        // kind of transparency.
        "r = close * 3.7 - 200\ng = bar_index % 4 == 0 ? na : open * 1.3\nt = bar_index % 5 == 0 ? 62.5 : bar_index % 5 == 1 ? 130.0 : bar_index % 5 == 2 ? -0.5 : 25.0\nc = color.rgb(r, g, 128.5, t)\nplot(close, color=c)\nplot(na(c) ? -1 : color.r(c) * 65536 + color.g(c) * 256 + color.b(c))\nplot(na(c) ? -1 : color.t(c))\n",
        "c = color.rgb(bar_index * 9, 300 - bar_index * 11, bar_index, bar_index * 7 - 20)\nplot(close, color=c)\nplot(color.t(c) + color.b(c))\nbgcolor(color.new(c, 90))\n",
        // A chain: channels of one color feed the next, as a script's own
        // gradient does.
        "a = color.new(color.teal, 0)\nb = bar_index % 7 == 0 ? tint : color.new(color.maroon, 0)\nk = (bar_index % 10) / 10.0\nmix = color.rgb(color.r(a) + (color.r(b) - color.r(a)) * k, color.g(a) + (color.g(b) - color.g(a)) * k, color.b(a) + (color.b(b) - color.b(a)) * k, bar_index % 3 == 0 ? 15 : 15.5)\nplot(close, color=mix)\nbarcolor(color.new(mix, 70))\n",
    ] {
        let source = format!("//@version=6\nindicator(\"Color calls\", overlay=true)\n{prelude}{body}");
        let program = compile_csil(&source);
        assert!(
            compile_program(&program).is_err(),
            "{body}: the fixture must need full colors"
        );
        let module = assert_source_matches(&source, &RunOptions::default(), &[0, 1, 2, 43]);
        let code = &program.main().code;
        assert!(
            module
                .layout
                .host_sites
                .iter()
                .any(|site| format!("{:?}", code[site.pc as usize]).contains("Color")),
            "{body}: a color call is a host site"
        );
    }
}

/// The packed lowering runs in the module: a host site first checks that the
/// run's instruction budget covers the bar's (the host could charge up to
/// that), and hands the run back when it does not. A color call on packed
/// operands makes no such check, because it makes no such call.
#[test]
fn a_color_call_on_packed_operands_does_not_call_the_host() {
    let source = |transparency: &str| {
        format!(
            "//@version=6\nindicator(\"Stay\")\ntint = input.color(#3366ff)\nt = {transparency}\nc = color.new(color.red, t)\nmix = color.rgb(color.r(c), color.g(c) + 1, color.b(c) + 2, t)\nplot(close, color = bar_index % 4 == 0 ? tint : mix)\nplot(color.t(mix) + color.b(mix))\n"
        )
    };
    let data = synthetic_data(9);
    // Enough for the whole run, and less than one bar may spend.
    let options = RunOptions {
        max_instructions_per_run: 4_000,
        ..RunOptions::default()
    };
    assert!(options.max_instructions_per_bar > options.max_instructions_per_run);

    // Whole transparencies on every bar: no call, so no hand-back.
    let whole = source("bar_index % 2 == 0 ? 20.0 : 35.0");
    let program = compile_csil(&whole);
    let module = compile_chart_wasm_program(&program).expect("compiles");
    let code = &program.main().code;
    let color_sites = module
        .layout
        .host_sites
        .iter()
        .filter(|site| format!("{:?}", code[site.pc as usize]).contains("Color"))
        .count();
    assert_eq!(
        (color_sites, module.layout.host_sites.len()),
        (7, 7),
        "color.new, color.rgb and five channel reads are the host sites"
    );
    let reference = run(&program, &data, &options);
    let chart = run_chart(&module, &program, &data, &options);
    assert_run_matches(&reference, &chart, 9, "whole transparencies");

    // A fractional one on bar 5: that bar's `color.new` is the host's.
    let fractional = source("bar_index == 5 ? 47.5 : 20.0");
    let program = compile_csil(&fractional);
    let module = compile_chart_wasm_program(&program).expect("compiles");
    let mut chart = Chart::new(&module);
    assert_eq!(chart.prepare(9), status::OK);
    chart.load(&module, &program, &data, &options, 9);
    chart.init(&request(&data, &options, 9)).expect("chartInit");
    assert_eq!(chart.run(), status::RUN_BUDGET_REROUTE);
    assert_eq!(u32_at(chart.memory(), header::OFF_ERR_BAR), 5);
    // With the budget out of the way the same run is the interpreter's.
    assert_source_matches(&fractional, &RunOptions::default(), &[9]);
}

/// A body over a thousand instructions runs as several functions
/// (`body/split.rs`), and a color call keeps both of its lowerings in
/// whichever piece it lands in.
#[test]
fn color_calls_in_a_split_body_match_the_interpreter() {
    let mut source = String::from(
        "//@version=6\nindicator(\"Split\", overlay=true)\ntint = input.color(#3366ff)\nvar float acc = 0.0\ncolor last = na\n",
    );
    for i in 0..90 {
        // Whole on most bars, fractional on some, per block.
        source.push_str(&format!(
            "if bar_index % {} == {}\n    c{i} = color.new(color.rgb({}, {}, {}), bar_index % 4 == 0 ? {}.5 : {})\n    acc += color.r(c{i}) + color.t(c{i})\n    last := c{i}\n",
            i % 5 + 2,
            i % 2,
            i * 2 % 256,
            255 - i,
            i * 7 % 256,
            i % 90,
            i % 100,
        ));
    }
    source.push_str("plot(acc, color = bar_index % 3 == 0 ? tint : last)\nbgcolor(color.new(nz(last, tint), 85))\n");
    let program = compile_csil(&source);
    assert!(
        program.main().code.len() > 1_000,
        "{} instructions do not split",
        program.main().code.len()
    );
    assert!(compile_program(&program).is_err(), "needs full colors");
    assert_source_matches(&source, &RunOptions::default(), &[0, 1, 41]);
}

/// A linked module writes a slot's per-bar cell only when something reads it
/// back. These are the readers besides a plain history read: an average and
/// a change sized by an input, a `varip` slot, and the collector, whose
/// roots are the `var` slots and the references read from an earlier bar.
#[test]
fn every_reader_of_a_slot_lane_keeps_it() {
    // An input-sized average and change over a slot nothing else reads back.
    let sized = "//@version=6\nindicator(\"Sized\")\nn = input.int(5, \"N\", minval=1, maxval=40)\nx = close * 2.0 + open\ny = x - low\nplot(ta.sma(x, n))\nplot(ta.change(y, n))\nplot(ta.sma(close, n) + ta.change(close, n))\n";
    let program = compile_csil(sized);
    assert!(!program.ta_input_bindings.is_empty(), "the length is bound");
    let module = compile_chart_wasm_program(&program).expect("compiles");
    for length in [1, 3, 5, 17] {
        let mut options = RunOptions::default();
        options
            .inputs
            .insert(program.inputs[0].id, InputValue::Int(length));
        let data = synthetic_data(46);
        let reference = run(&program, &data, &options);
        let chart = run_chart(&module, &program, &data, &options);
        assert_run_matches(&reference, &chart, 46, &format!("length {length}"));
    }

    // `varip` carries its value forward like `var` over history.
    assert_source_matches(
        "//@version=6\nindicator(\"Varip\")\nvarip float seen = 0.0\nseen := seen + close\nvarip int ticks = 0\nticks += 1\nplot(seen)\nplot(ticks)\nplot(seen[1])\n",
        &RunOptions::default(),
        &[0, 1, 2, 31],
    );

    // The collector. The run charges 840 elements, far more than its limit
    // allows at once, so it finishes only if the garbage of every bar is
    // collected, and it is right only if the `var` arrays its lanes still
    // name are not: `var` arrays, and objects and temporaries made on
    // every bar.
    assert_source_matches(
        "//@version=6\nindicator(\"Collector\", overlay=true)\ntype Settings\n    float source\n    int neighbors\ntype Features\n    array<float> f1\n    array<float> f2\nsize_of(float x) =>\n    array.size(array.from(x))\nvar f1 = array.new_float()\nvar f2 = array.new_float()\nsettings = Settings.new(close, 8)\narray.push(f1, close)\narray.push(f2, open)\nfeatures = Features.new(f1, f2)\nlast = array.get(features.f1, array.size(features.f1) - 1)\nplot(settings.source + last + size_of(high))\n",
        &RunOptions {
            max_collection_elements: 320,
            ..RunOptions::default()
        },
        &[120],
    );

    // With a host site: a label on every bar, which the linked host collects
    // when the module asks. Wherever the interpreter finishes, the module
    // must, with the same result; and it must at a limit that needs a
    // collection, since sixty bars charge more than 200 elements.
    let labels = "//@version=6\nindicator(\"Labels\", overlay=true, max_labels_count=5)\ntype Settings\n    float source\n    int neighbors\nvar f1 = array.new_float()\nsettings = Settings.new(close, 8)\narray.push(f1, close)\nlabel.new(bar_index, settings.source, \"x\")\nplot(array.sum(f1))\n";
    let program = compile_csil(labels);
    let module = compile_chart_wasm_program(&program).expect("compiles");
    assert!(
        !module.layout.host_sites.is_empty(),
        "the label is a host site"
    );
    let data = synthetic_data(60);
    let mut finished = Vec::new();
    for limit in [160, 180, 200, 230, 260, 1_000_000] {
        let options = RunOptions {
            max_collection_elements: limit,
            ..RunOptions::default()
        };
        let reference = run(&program, &data, &options);
        if !reference.errors.is_empty() {
            continue;
        }
        let chart = run_chart(&module, &program, &data, &options);
        assert_run_matches(&reference, &chart, 60, &format!("labels, limit {limit}"));
        finished.push(limit);
    }
    assert!(
        finished.iter().any(|&limit| limit <= 260),
        "no tight limit finishes: {finished:?}"
    );
}

/// The layout says how many host sites are inside a loop, which bounds what
/// a module's host calls can cost: the rest run at most once a bar.
#[test]
fn the_layout_counts_the_host_sites_inside_loops() {
    let sites = |body: &str| {
        let source = format!("//@version=6\nindicator(\"Loops\", overlay=true)\n{body}");
        let program = compile_csil(&source);
        let linked = compile_chart_wasm_program(&program).expect("compiles");
        let imported = compile_program(&program).expect("compiles");
        assert_eq!(
            (
                linked.layout.host_sites.len(),
                linked.layout.host_sites_in_loops
            ),
            (
                imported.layout.host_sites.len(),
                imported.layout.host_sites_in_loops
            ),
            "{body}: both tiers plan the same sites"
        );
        assert_source_matches(&source, &RunOptions::default(), &[0, 1, 23]);
        (
            linked.layout.host_sites.len() as u32,
            linked.layout.host_sites_in_loops,
        )
    };
    // The loop is all arithmetic; the label is made once a bar.
    let (all, looped) = sites(
        "float total = 0.0\nfor i = 0 to 20\n    total += math.sin(i * close)\nif bar_index % 5 == 0\n    label.new(bar_index, high, str.tostring(total))\nplot(total)\n",
    );
    assert!(all > 0, "the label and its text are host sites");
    assert_eq!(looped, 0);
    // The text is built inside the loop.
    let (all, looped) = sites(
        "string text = \"\"\nfor i = 0 to 3\n    text := text + str.tostring(i)\nif bar_index % 5 == 0\n    label.new(bar_index, high, text)\nplot(close)\n",
    );
    assert!(looped > 0 && looped < all, "{looped} of {all}");
    // No host site at all.
    assert_eq!(sites("plot(ta.sma(close, 5))\n"), (0, 0));
}

/// A program whose colors do fit a packed cell keeps them native: no color
/// instruction becomes a host site because the host is linked.
#[test]
fn packable_colors_stay_native() {
    let source = "//@version=6\nindicator(\"Packed\")\nc = close > open ? color.new(color.green, 20) : color.rgb(255, 0, 0, 40)\nplot(close, color=c)\nplot(color.t(c))\nbgcolor(color.from_gradient(close, 99, 103, #000000, #ffffff))\n";
    let module = assert_source_matches(source, &RunOptions::default(), &[0, 1, 33]);
    assert!(
        module.layout.host_sites.is_empty(),
        "{:?}",
        module.layout.host_sites
    );
}

/// A strategy's broker is the interpreter's, inside the module: orders,
/// position reads, fills, equity, the report and the fill alerts.
#[test]
fn strategies_match_the_interpreter() {
    for body in [
        "fast = ta.sma(close, 3)\nslow = ta.sma(close, 8)\nif ta.crossover(fast, slow)\n    strategy.entry(\"L\", strategy.long)\nif ta.crossunder(fast, slow)\n    strategy.close(\"L\")\nplot(strategy.position_size)\nplot(strategy.equity)\n",
        "if bar_index % 10 == 2\n    strategy.entry(\"S\", strategy.short, qty=2, alert_message=\"short \" + str.tostring(close))\nif bar_index % 10 == 7\n    strategy.exit(\"X\", \"S\", profit=50, loss=50)\nplot(strategy.netprofit)\nplot(strategy.closedtrades)\n",
        "if bar_index == 5\n    strategy.order(\"A\", strategy.long, limit=close * 0.99)\nif bar_index == 20\n    strategy.close_all(comment=\"done\")\nplot(strategy.opentrades)\n",
    ] {
        let source = format!(
            "//@version=6\nstrategy(\"Broker\", overlay=true, initial_capital=10000, commission_type=strategy.commission.percent, commission_value=0.1)\n{body}"
        );
        let module = assert_source_matches(&source, &RunOptions::default(), &[0, 1, 64]);
        assert!(module.layout.host_broker, "the linked host runs the broker");
    }
}

/// A script's own error stops the run on the bar it happens, with the
/// interpreter's structured error and the bars before it intact.
#[test]
fn a_runtime_error_is_the_interpreters() {
    let source = "//@version=6\nindicator(\"Error\")\nif bar_index == 6\n    runtime.error(\"stop at \" + str.tostring(bar_index))\nplot(close)\n";
    let program = compile_csil(source);
    let module = compile_chart_wasm_program(&program).expect("compiles");
    let data = synthetic_data(12);
    let options = RunOptions::default();
    let reference = run(&program, &data, &options);
    assert_eq!(reference.errors.len(), 1);
    let chart = run_chart(&module, &program, &data, &options);
    assert_eq!(chart.status, 5, "a host error");
    assert_eq!(chart.err_bar, 6);
    assert_eq!(chart.error, wire!(&reference.errors[0]));
    for bar in 0..6 {
        let Value::Float(expected) = reference.plots[0].values[bar] else {
            panic!("bar {bar}");
        };
        assert_eq!(
            chart.plots[0][bar].to_bits(),
            expected.to_bits(),
            "bar {bar}"
        );
    }
}

/// How the runs of one sweep stood to the interpreter's, and where the
/// module stopped itself.
#[derive(Debug, Default)]
struct Sweep {
    finished: usize,
    stopped: usize,
    handed_back: usize,
    /// The instructions the module stopped at with the interpreter's error,
    /// by name.
    stops: std::collections::BTreeSet<String>,
    /// Those instructions' offsets.
    stop_pcs: std::collections::BTreeSet<u32>,
    /// The kinds of error it stopped with, by name.
    errors: std::collections::BTreeSet<String>,
    /// The kinds of the interpreter's errors in the runs handed back.
    handed_back_errors: std::collections::BTreeSet<String>,
    /// Why the host handed a run back, when it said.
    reasons: std::collections::BTreeSet<String>,
}

/// Why a linked host hands back an instruction budget stop of the module's
/// own code, and one of a hosted instruction
/// (`pinewasm_vm::aot_host`).
const OWN_BUDGET_STOP: &str =
    "an instruction budget stop in the module's own code is the interpreter's to report";
const HOSTED_BUDGET_STOP: &str =
    "an instruction budget stop takes its source location from the interpreter's reference run";

/// An instruction's name: its variant, without its operands.
fn instruction_name(instruction: &pinewasm_csil::CsilInstr) -> String {
    format!("{instruction:?}")
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect()
}

/// Run `module` under each of `cases` and require every run to stand to the
/// interpreter's in one of the three ways [`agreement`] allows.
fn sweep(
    module: &AotModule,
    program: &CsilProgram,
    data: &MarketData,
    cases: impl IntoIterator<Item = RunOptions>,
    context: &str,
) -> Sweep {
    let loaded = Loaded::new(module);
    let code = &program.main().code;
    let mut seen = Sweep::default();
    for options in cases {
        let reference = run(program, data, &options);
        let chart = run_loaded(&loaded, module, program, data, &options);
        let limits = format!(
            "{context}: string budget {}, {} instructions a bar, {} a run",
            options.max_string_bytes_per_run,
            options.max_instructions_per_bar,
            options.max_instructions_per_run
        );
        match assert_agrees(&reference, &chart, &limits) {
            Agreement::Finished => seen.finished += 1,
            Agreement::Stopped => {
                seen.stopped += 1;
                seen.stops
                    .insert(instruction_name(&code[chart.err_pc as usize]));
                seen.stop_pcs.insert(chart.err_pc);
                seen.errors
                    .insert(format!("{:?}", reference.errors[0].kind));
            }
            Agreement::HandedBack => {
                seen.handed_back += 1;
                seen.handed_back_errors
                    .insert(format!("{:?}", reference.errors[0].kind));
                if let Some(reason) = chart.mismatch.as_str() {
                    seen.reasons.insert(reason.to_string());
                }
            }
        }
    }
    seen
}

/// The least limit the interpreter finishes `program` under, where
/// `limited` gives the options of a limit and a run that finishes under one
/// limit finishes under every larger one.
fn least_limit(
    program: &CsilProgram,
    data: &MarketData,
    limited: impl Fn(u64) -> RunOptions,
) -> u64 {
    let finishes = |limit: u64| run(program, data, &limited(limit)).errors.is_empty();
    let mut high = 1u64;
    while !finishes(high) {
        assert!(high < 1 << 40, "the run finishes under no limit");
        high *= 2;
    }
    let mut low = 0u64;
    while low < high {
        let middle = low + (high - low) / 2;
        if finishes(middle) {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    low
}

/// The string bytes the interpreter reserves over a whole run: the smallest
/// budget it finishes under.
fn string_bytes_reserved(program: &CsilProgram, data: &MarketData, options: &RunOptions) -> usize {
    least_limit(program, data, |budget| RunOptions {
        max_string_bytes_per_run: budget as usize,
        ..options.clone()
    }) as usize
}

/// Sweep the string budget of `program` from nothing to one byte more than
/// the run reserves: every budget when the run reserves at most `dense`
/// bytes, and otherwise a quarter of `dense` budgets at either end and half
/// of it spread between. The interpreter finishes under the last two and
/// stops under every other.
fn sweep_string_budget(
    module: &AotModule,
    program: &CsilProgram,
    data: &MarketData,
    options: &RunOptions,
    dense: usize,
    context: &str,
) -> Sweep {
    let reserved = string_bytes_reserved(program, data, options);
    assert!(reserved > 0, "{context}: the run reserves no string bytes");
    let limited = |budget: usize| RunOptions {
        max_string_bytes_per_run: budget,
        ..options.clone()
    };
    let mut budgets: Vec<usize> = if reserved <= dense {
        (0..=reserved + 1).collect()
    } else {
        // A spread can miss an instruction that reserves a few bytes, so it
        // is joined by the first budget that stops the interpreter at each
        // kind of instruction.
        let mut kinds = std::collections::BTreeSet::new();
        let first_of_each_kind: Vec<usize> = (0..reserved)
            .filter(|&budget| {
                let stopped = run(program, data, &limited(budget)).errors[0]
                    .instruction_offset
                    .expect("the instruction that stopped the run");
                kinds.insert(instruction_name(&program.main().code[stopped as usize]))
            })
            .collect();
        (0..dense / 4)
            .chain((0..reserved).step_by(reserved / (dense / 2)))
            .chain(reserved - dense / 4..=reserved + 1)
            .chain(first_of_each_kind)
            .collect()
    };
    budgets.sort_unstable();
    budgets.dedup();
    let stops = budgets.iter().filter(|&&budget| budget < reserved).count();
    let seen = sweep(
        module,
        program,
        data,
        budgets.iter().map(|&budget| limited(budget)),
        context,
    );
    assert_eq!(seen.finished, 2, "{context}: {seen:?}");
    assert_eq!(
        seen.stopped + seen.handed_back,
        stops,
        "{context}: {seen:?}"
    );
    seen
}

/// The interpreter reserves string bytes at instructions a linked module
/// runs itself: where it pushes a string constant, where an
/// `alertcondition` fires, and where it copies an object's string fields.
/// The module keeps the budget with its host, so it stops under every
/// budget the interpreter stops under, on the same bar, at the same
/// instruction, with the interpreter's error, and finishes under the rest.
/// None of these runs is handed back.
#[test]
fn the_string_budget_stops_a_linked_module_where_it_stops_the_interpreter() {
    let split_body = {
        let mut source = String::from(
            "//@version=6\nindicator(\"Split\", overlay=true)\ntype Tag\n    string name\n    float value\nvar float acc = 0.0\nstring last = na\nif bar_index % 3 == 0\n    label.new(bar_index, high, \"bar \" + str.tostring(bar_index))\n",
        );
        for i in 0..150 {
            source.push_str(&format!(
                "if bar_index % {} == {}\n    last := \"v{i}\"\n    acc += last == \"v{}\" ? {i} : 1\n",
                i % 4 + 2,
                i % 2,
                i / 2,
            ));
        }
        source.push_str("copied = Tag.copy(Tag.new(last, acc))\nplot(copied.value)\n");
        source
    };
    // (what the case holds, its source, its bars, the instructions the
    // sweep must see the module stop at, and whether it has host sites)
    let cases: [(&str, &str, usize, &[&str], bool); 10] = [
        (
            "constants compared in a loop",
            "//@version=6\nindicator(\"Constants\")\nmode = input.string(\"slow\", options=[\"fast\", \"slow\"])\nfloat total = 0.0\nfor i = 0 to 3\n    total += mode == \"fast\" ? i * close : mode == \"slow\" ? i : 0\nside = bar_index % 2 == 0 ? \"even\" : \"odd\"\nplot(total + (side == \"even\" ? 1 : 0))\n",
            24,
            &["PushStr"],
            false,
        ),
        (
            "alert conditions with a literal, an omitted and a computed message",
            "//@version=6\nindicator(\"Alerts\")\nnote = bar_index % 2 == 0 ? \"even bar\" : \"odd\"\nalertcondition(close > open, \"Up\", \"closed up\")\nalertcondition(close < open, \"Down\")\nalertcondition(close > open[1], \"Mixed\", note)\nplot(close)\n",
            16,
            &["PushStr", "AlertCondition"],
            false,
        ),
        (
            "an alert condition whose message is na",
            "//@version=6\nindicator(\"Na\")\nstring missing = na\nalertcondition(close > open, \"Up\", missing)\nplot(close)\n",
            12,
            &["PushStr", "AlertCondition"],
            false,
        ),
        (
            "text built on the host",
            "//@version=6\nindicator(\"Text\", overlay=true)\ntext = \"bar \" + str.tostring(bar_index) + (close > open ? \" up\" : \" down\")\nif bar_index % 3 == 0\n    label.new(bar_index, high, text)\nif bar_index % 4 == 0\n    log.info(\"close {0}\", close)\nif bar_index % 5 == 0\n    alert(str.format(\"{0} at {1}\", \"bar\", bar_index), alert.freq_all)\nplot(str.length(text))\n",
            8,
            &["PushStr", "StrConcat", "CallPure", "Log", "Alert"],
            true,
        ),
        (
            "collections of strings",
            "//@version=6\nindicator(\"Collections\")\nnames = array.new_string(0)\narray.push(names, \"a\" + str.tostring(bar_index % 3))\narray.push(names, \"tail\")\njoined = array.join(names, \",\")\nm = map.new<string, float>()\nmap.put(m, joined, close)\nplot(map.size(m) + str.length(joined) + array.size(str.split(joined, \",\")))\n",
            8,
            &["PushStr", "StrConcat", "CallCollection"],
            true,
        ),
        (
            "objects with string fields, copied on the native heap",
            "//@version=6\nindicator(\"Objects\")\ntype Tag\n    string name\n    float value\n    string note\nt = Tag.new(bar_index % 2 == 0 ? \"even\" : \"odd\", close, bar_index % 3 == 0 ? na : \"n\")\nu = Tag.copy(t)\nplot(u.value)\nplot(u.name == \"even\" ? 1 : 0)\n",
            12,
            &["PushStr", "CopyObject"],
            false,
        ),
        (
            "a strategy's order names and comments",
            "//@version=6\nstrategy(\"Orders\", overlay=true)\nif bar_index % 5 == 1\n    strategy.entry(\"Long \" + str.tostring(bar_index % 2), strategy.long, comment=\"in\")\nif bar_index % 5 == 4\n    strategy.close_all(comment=\"out\")\nplot(strategy.position_size)\n",
            14,
            &["PushStr", "StrConcat", "Order"],
            true,
        ),
        (
            "a program carried into float code",
            "//@version=6\nindicator(\"Carried\")\nn = input.int(7, \"N\")\nvar int acc = 0\nacc := acc + n / 2\nside = bar_index % 2 == 0 ? \"a\" : \"b\"\ns = str.tostring(volume) + \"x\"\nalertcondition(side == \"a\", \"A\", \"side a\")\nplot(acc + str.length(s))\n",
            10,
            &["PushStr", "StrConcat", "AlertCondition"],
            true,
        ),
        (
            "a body split into several functions",
            &split_body,
            7,
            &["PushStr", "CallPure", "CopyObject"],
            true,
        ),
        (
            "an object kept across bars whose string field the host built",
            "//@version=6\nindicator(\"Kept\")\ntype Tag\n    string name\n    float value\nvar Tag kept = Tag.new(\"first\", 0.0)\nif bar_index % 2 == 0\n    kept := Tag.new(str.tostring(bar_index % 3), close)\nu = Tag.copy(kept)\nplot(u.value + str.length(u.name))\n",
            10,
            &["PushStr", "CallPure", "CopyObject"],
            true,
        ),
    ];
    for (what, source, bars, stops, hosted) in cases {
        let program = compile_csil(source);
        let split = program.main().code.len() > 1_000;
        let module =
            compile_chart_wasm_program(&program).unwrap_or_else(|error| panic!("{what}: {error}"));
        assert_eq!(
            !module.layout.host_sites.is_empty(),
            hosted,
            "{what}: host sites {:?}",
            module.layout.host_sites
        );
        let data = synthetic_data(bars);
        // A run of the split body costs some thirty times a run of any
        // other, so its sweep tries fewer budgets.
        let dense = if split { 128 } else { 2_048 };
        let seen = sweep_string_budget(
            &module,
            &program,
            &data,
            &RunOptions::default(),
            dense,
            what,
        );
        assert_eq!(seen.handed_back, 0, "{what}: {seen:?}");
        for stop in stops {
            assert!(
                seen.stops.contains(*stop),
                "{what}: no run stopped at a {stop}: {:?}",
                seen.stops
            );
        }
        // An alert condition reserves its message's bytes where it fires:
        // every one of them is where some budget ends.
        for (pc, instruction) in program.main().code.iter().enumerate() {
            let native = !module
                .layout
                .host_sites
                .iter()
                .any(|site| site.pc as usize == pc);
            if native && matches!(instruction, pinewasm_csil::CsilInstr::AlertCondition) {
                assert!(
                    seen.stop_pcs.contains(&(pc as u32)),
                    "{what}: no run stopped at the alert condition at {pc}"
                );
            }
        }
    }

    // What the cases above rest on: the object cases copy on the native
    // heap, the carried case needs the carry, and the split case is split.
    for case in [5, 8, 9] {
        let program = compile_csil(cases[case].1);
        assert!(
            compile_chart_wasm_program(&program)
                .expect("compiles")
                .layout
                .native_heap,
            "{}",
            cases[case].0
        );
    }
    assert!(pinewasm_aot::analysis::analyze(&compile_csil(cases[7].1)).is_err());
    assert!(compile_csil(&split_body).main().code.len() > 1_000);
}

/// A string input the request overrides with a text the program does not
/// hold gets a handle of its own from the linked host. The budget is the
/// same whatever the input holds, since reading an input reserves nothing.
#[test]
fn the_string_budget_is_kept_under_string_input_overrides() {
    let source = "//@version=6\nindicator(\"Inputs\")\nmode = input.string(\"slow\", options=[\"fast\", \"slow\"])\nname = input.string(\"plain\")\nalertcondition(close > open, \"Up\", \"up\")\nplot(mode == \"fast\" ? 1 : name == \"plain\" ? 2 : 3)\n";
    let program = compile_csil(source);
    let module = compile_chart_wasm_program(&program).expect("compiles");
    assert!(module.layout.host_sites.is_empty());
    let data = synthetic_data(9);
    let mut reserved = Vec::new();
    for name in [None, Some("a text the constant pool does not hold")] {
        let mut options = RunOptions::default();
        if let Some(name) = name {
            options
                .inputs
                .insert(program.inputs[1].id, InputValue::String(name.to_string()));
        }
        let seen = sweep_string_budget(&module, &program, &data, &options, 2_048, source);
        assert_eq!(seen.handed_back, 0, "{seen:?}");
        reserved.push(string_bytes_reserved(&program, &data, &options));
    }
    assert_eq!(reserved[0], reserved[1]);
}

/// Every linked fixture under three string budgets: a module finishes
/// where the interpreter finishes and stops where it stops, except the fixture
/// that is declined before emission because its repeated table constructor
/// needs alias invalidation in module memory.
#[test]
fn every_public_fixture_keeps_the_string_budget() {
    let mut files = Vec::new();
    collect_pine_files(Path::new("../../tests/compile-pass"), &mut files);
    files.sort();
    let data = synthetic_data(48);
    let (mut finished, mut stopped, mut rerouted, mut refused) =
        (0usize, 0usize, Vec::new(), Vec::new());
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let source = std::fs::read_to_string(path).expect("fixture");
        let result = compile(&source, CompileOptions::default());
        let Some(program) = result.program else {
            continue;
        };
        if result
            .diagnostics
            .iter()
            .any(pinewasm_span::Diagnostic::is_error)
        {
            continue;
        }
        let module = match compile_chart_wasm_program(&program) {
            Ok(module) => module,
            Err(error) => {
                refused.push(format!("{name}: {error}"));
                continue;
            }
        };
        let loaded = Loaded::new(&module);
        for budget in [0, 96, 4_096] {
            let options = RunOptions {
                max_string_bytes_per_run: budget,
                ..RunOptions::default()
            };
            let reference = run(&program, &data, &options);
            let chart = run_loaded(&loaded, &module, &program, &data, &options);
            if chart.status == status::ABI_MISMATCH && !chart.mismatch.is_null() {
                if !rerouted.contains(&name) {
                    rerouted.push(name.clone());
                }
                continue;
            }
            match assert_agrees(
                &reference,
                &chart,
                &format!("{name}, string budget {budget}"),
            ) {
                Agreement::Finished => finished += 1,
                Agreement::Stopped => stopped += 1,
                Agreement::HandedBack => panic!("{name}, string budget {budget}: handed back"),
            }
        }
    }
    eprintln!(
        "[chart string budgets] finished={finished} stopped={stopped} rerouted={rerouted:?} refused={refused:?}"
    );
    assert!(rerouted.is_empty());
    assert_eq!(
        refused,
        ["drawings.pine: unsupported opcode for AOT: table position replacement requires interpreter"]
    );
    assert!(
        finished >= 100 && stopped >= 15,
        "finished {finished}, stopped {stopped}"
    );
}

/// A bar that spends its instructions and a run that spends its string
/// bytes stop the interpreter at whichever instruction comes first. A module
/// stops with it on the string budget, with its error, and hands an
/// instruction budget stop of its own code back, whichever of its two code
/// paths the bar is on: the one that charges a window of instructions at
/// once, or the one it falls back to when the bar's budget does not cover
/// the window. The programs carry no unoptimized form, so a hosted
/// instruction that spends the bar's budget stops with the interpreter's
/// error too.
#[test]
fn the_string_and_instruction_budgets_stop_in_the_interpreters_order() {
    fn per_bar(limit: u64) -> RunOptions {
        RunOptions {
            max_instructions_per_bar: limit,
            ..RunOptions::default()
        }
    }
    fn per_run(limit: u64) -> RunOptions {
        RunOptions {
            max_instructions_per_run: limit,
            ..RunOptions::default()
        }
    }
    /// The options of an instruction budget.
    type Budget = fn(u64) -> RunOptions;
    for (source, hosted) in [
        (
            "//@version=6\nindicator(\"Both\")\nmode = input.string(\"slow\", options=[\"fast\", \"slow\"])\nfloat total = 0.0\nfor i = 0 to 5\n    total += mode == \"fast\" ? i * close : mode == \"slow\" ? i : 0\nalertcondition(total > 2, \"High\", \"total is high\")\nplot(total)\n",
            false,
        ),
        (
            "//@version=6\nindicator(\"Both, hosted\", overlay=true)\ntext = \"bar \" + str.tostring(bar_index) + (close > open ? \" up\" : \" down\")\nif bar_index % 2 == 0\n    label.new(bar_index, high, text)\nplot(str.length(text))\n",
            true,
        ),
    ] {
        let mut program = compile_csil(source);
        program.functions[0].budget_reference = None;
        let module = compile_chart_wasm_program(&program).expect("compiles");
        assert_eq!(!module.layout.host_sites.is_empty(), hosted, "{source}");
        let data = synthetic_data(5);
        let strings = string_bytes_reserved(&program, &data, &RunOptions::default());
        // A module with host sites hands a run back at the first of them
        // once less is left of the run's budget than of the bar's, whether
        // or not the interpreter would finish (§6), so only the module
        // without one meets the run's budget here.
        let budgets: &[Budget] = if hosted {
            &[per_bar]
        } else {
            &[per_bar, per_run]
        };
        for &budget in budgets {
            let instructions = least_limit(&program, &data, budget);
            assert!(strings > 0 && instructions > 0, "{source}");
            // Some forty of each budget, from none to one more than enough.
            let spread = |enough: u64| {
                (0..=enough + 1)
                    .step_by((enough as usize / 40).max(1))
                    .chain([enough, enough + 1])
            };
            let cases = spread(instructions).flat_map(|limit| {
                spread(strings as u64).map(move |bytes| RunOptions {
                    max_string_bytes_per_run: bytes as usize,
                    ..budget(limit)
                })
            });
            let seen = sweep(&module, &program, &data, cases, source);
            assert!(
                seen.finished >= 4 && seen.stopped > 100 && seen.handed_back > 100,
                "{seen:?}\n{source}"
            );
            // Every string budget stop is the module's own.
            assert!(seen.errors.contains("StringLimitExceeded"), "{seen:?}");
            assert!(
                seen.handed_back_errors
                    .iter()
                    .all(|kind| kind.ends_with("InstructionLimitExceeded")),
                "{seen:?}\n{source}"
            );
            assert_eq!(
                seen.reasons.iter().map(String::as_str).collect::<Vec<_>>(),
                [OWN_BUDGET_STOP],
                "{source}"
            );
            // And so is the stop of a hosted instruction that spent the
            // bar's budget.
            assert_eq!(
                seen.errors.contains("InstructionLimitExceeded"),
                hosted,
                "{seen:?}\n{source}"
            );
        }
    }
}

/// An instruction budget stop is the interpreter's to report, for two
/// reasons. The interpreter does not always report it at the stopped
/// instruction: when the program carries its unoptimized form, the
/// interpreter runs that form too and, if it stops on the same bar with the
/// same results, reports where that run stopped. And the error depends on
/// the order of an instruction's own checks. So a module hands back every
/// such stop its own code makes. A hosted instruction that spends the bar's
/// budget is the interpreter's own code: its error stands, unless the
/// program carries that form, and then it is handed back too.
#[test]
fn an_instruction_budget_stop_is_handed_back_or_the_interpreters() {
    fn per_bar(limit: u64) -> RunOptions {
        RunOptions {
            max_instructions_per_bar: limit,
            ..RunOptions::default()
        }
    }
    fn per_run(limit: u64) -> RunOptions {
        RunOptions {
            max_instructions_per_run: limit,
            ..RunOptions::default()
        }
    }
    /// The options of one budget, and which budget that is.
    type Budget = (&'static str, fn(u64) -> RunOptions);
    for (source, bars, hosted) in [
        (
            "//@version=6\nindicator(\"Budget\")\nfloat total = 0.0\nfor i = 0 to bar_index * 3\n    total += i\nplot(total)\n",
            12,
            false,
        ),
        (
            "//@version=6\nindicator(\"Hosted budget\", overlay=true)\nstring text = \"\"\nfor i = 0 to bar_index\n    text := text + str.tostring(i)\nif bar_index % 4 == 0\n    label.new(bar_index, high, text)\nplot(str.length(text))\n",
            10,
            true,
        ),
    ] {
        let program = compile_csil(source);
        assert!(
            program.main().budget_reference.is_some(),
            "the fixture must carry its unoptimized form:\n{source}"
        );
        let data = synthetic_data(bars);
        let budgets = (0..120)
            .chain((120..9_000).step_by(37))
            .chain([100_000, 10_000_000]);

        // A module with host sites hands a run back at the first of them
        // once less is left of the run's budget than of the bar's, whether
        // or not the interpreter would finish (§6). Only the module without
        // one is swept over the run's budget.
        let kinds: &[Budget] = if hosted {
            &[("a bar", per_bar)]
        } else {
            &[("a bar", per_bar), ("a run", per_run)]
        };

        // As compiled: every stop is handed back. A hosted instruction's
        // gives the second reason.
        let module = compile_chart_wasm_program(&program).expect("compiles");
        assert_eq!(!module.layout.host_sites.is_empty(), hosted, "{source}");
        for &(kind, options) in kinds {
            let seen = sweep(&module, &program, &data, budgets.clone().map(options), source);
            assert!(
                seen.finished > 0 && seen.handed_back > 20 && seen.stopped == 0,
                "{kind}: {seen:?}\n{source}"
            );
            let reasons: Vec<&str> = seen.reasons.iter().map(String::as_str).collect();
            if hosted {
                assert_eq!(reasons, [OWN_BUDGET_STOP, HOSTED_BUDGET_STOP], "{kind}");
            } else {
                assert_eq!(reasons, [OWN_BUDGET_STOP], "{kind}");
            }
        }

        // Without the unoptimized form a hosted instruction's stop stands,
        // with the interpreter's error, and the module's own are still
        // handed back.
        let mut plain = program.clone();
        plain.functions[0].budget_reference = None;
        let module = compile_chart_wasm_program(&plain).expect("compiles");
        for &(kind, options) in kinds {
            let seen = sweep(&module, &plain, &data, budgets.clone().map(options), source);
            assert!(
                seen.finished > 0 && seen.handed_back > 20,
                "{kind}: {seen:?}\n{source}"
            );
            assert_eq!(
                seen.reasons.iter().map(String::as_str).collect::<Vec<_>>(),
                [OWN_BUDGET_STOP],
                "{kind}"
            );
            if hosted {
                assert!(seen.stopped > 20, "{kind}: {seen:?}\n{source}");
                let code = &plain.main().code;
                for &pc in &seen.stop_pcs {
                    assert!(
                        module.layout.host_sites.iter().any(|site| site.pc == pc),
                        "{kind}: the module stopped at {:?}, which is no host site",
                        code[pc as usize]
                    );
                }
            } else {
                assert_eq!(seen.stopped, 0, "{kind}: {seen:?}\n{source}");
            }
        }
    }

    // Find a budget that exercises source remapping in each fixture.
    // Instruction costs change as the compiler evolves, so a fixed budget
    // need not keep stopping where the interpreter run reports another span.
    let drawings_source =
        std::fs::read_to_string(Path::new("../../tests/compile-pass/drawings.pine"))
            .expect("drawings fixture");
    let drawings = compile_csil(&drawings_source);
    assert_eq!(
        compile_chart_wasm_program(&drawings).unwrap_err(),
        AotError::UnsupportedOpcode("table position replacement requires interpreter".into())
    );

    let fixture = "loops.pine";
    let source = std::fs::read_to_string(Path::new("../../tests/compile-pass").join(fixture))
        .expect("fixture");
    let program = compile_csil(&source);
    let module = compile_chart_wasm_program(&program).expect("compiles");
    let data = synthetic_data(48);
    let (options, reference) = (1..=1_000)
        .find_map(|limit| {
            let options = per_bar(limit);
            let reference = run(&program, &data, &options);
            let error = reference.errors.first()?;
            if error.kind != pinewasm_vm::RuntimeErrorKind::InstructionLimitExceeded {
                return None;
            }
            let reported = error.span?;
            let stopped = program.main().source_map[error.instruction_offset? as usize];
            ((stopped.start, stopped.end) != (reported.start, reported.end))
                .then_some((options, reference))
        })
        .unwrap_or_else(|| panic!("{fixture}: no budget exercises source remapping"));
    let chart = run_chart(&module, &program, &data, &options);
    assert_agrees(&reference, &chart, fixture);
    assert_eq!(chart.status, status::ABI_MISMATCH, "{fixture}");
    assert!(
        chart.mismatch == json!(OWN_BUDGET_STOP) || chart.mismatch == json!(HOSTED_BUDGET_STOP),
        "{fixture}: {}",
        chart.mismatch
    );
    assert_eq!(chart.error, Json::Null, "{fixture}");
}

/// The interpreter looks at the object a copy is given before it charges
/// for the copy's fields, and the module's code charges first. When the
/// object is `na` and the bar's budget covers the copy but not its fields,
/// the interpreter reports the missing object where the module's code runs
/// out of budget. That stop is handed back like every instruction budget
/// stop of the module's own code, so the interpreter's error is the one
/// reported, under every budget.
#[test]
fn a_copy_of_a_missing_object_is_the_interpreters_to_report_under_any_budget() {
    let source = "//@version=6\nindicator(\"Copy\")\ntype Tag\n    float a\n    float b\n    float c\n    float d\nTag t = bar_index % 2 == 1 ? Tag.new(close, open, high, low) : na\nu = Tag.copy(t)\nplot(u.a)\n";
    let mut program = compile_csil(source);
    program.functions[0].budget_reference = None;
    let module = compile_chart_wasm_program(&program).expect("compiles");
    assert!(
        module.layout.native_heap && module.layout.host_sites.is_empty(),
        "the copy must be the module's own"
    );
    let data = synthetic_data(6);
    let seen = sweep(
        &module,
        &program,
        &data,
        (0..60).map(|limit| RunOptions {
            max_instructions_per_bar: limit,
            ..RunOptions::default()
        }),
        source,
    );
    // The first bar's object is missing, so no budget finishes, and every
    // run is the interpreter's to report.
    assert_eq!((seen.finished, seen.stopped), (0, 0), "{seen:?}");
    assert_eq!(
        seen.handed_back_errors
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["InstructionLimitExceeded", "NaFieldAccess"],
        "{seen:?}"
    );
}

/// A history offset computed at run time that passes `max_bars_back` stops
/// the run in the module's own code. The linked host builds the
/// interpreter's error for it, which names the offset and the limit.
#[test]
fn a_history_stop_is_the_interpreters() {
    let fixture = std::fs::read_to_string("../../tests/compile-pass/dynamic_history_offset.pine")
        .expect("fixture");
    for (source, bars, message, carried) in [
        (
            fixture.as_str(),
            600,
            "history offset 501 exceeds the max_bars_back limit of 500",
            false,
        ),
        // A series of the script's own, read past a limit the script sets.
        (
            "//@version=6\nindicator(\"Own series\", max_bars_back=20)\nspread = high - low\nplot(spread[bar_index % 30])\n",
            64,
            "history offset 21 exceeds the max_bars_back limit of 20",
            false,
        ),
        // An offset the module carries as a float (a kept quotient): 3.5
        // bars times ten.
        (
            "//@version=6\nindicator(\"Carried\", max_bars_back=20)\nn = input.int(7)\noff = n / 2 * (bar_index < 30 ? 2 : 10)\nplot(close[off])\n",
            40,
            "history offset 35 exceeds the max_bars_back limit of 20",
            true,
        ),
    ] {
        let program = compile_csil(source);
        assert_eq!(
            pinewasm_aot::analysis::analyze(&program).is_err(),
            carried,
            "{source}"
        );
        let module = compile_chart_wasm_program(&program).expect("compiles");
        assert!(module.layout.host_sites.is_empty(), "{source}");
        let data = synthetic_data(bars);
        let options = RunOptions::default();
        let reference = run(&program, &data, &options);
        assert_eq!(
            reference.errors[0].kind,
            pinewasm_vm::RuntimeErrorKind::HistoryBufferLimitExceeded
        );
        assert_eq!(reference.errors[0].message, message);
        let chart = run_chart(&module, &program, &data, &options);
        assert_eq!(
            assert_agrees(&reference, &chart, source),
            Agreement::Stopped
        );
        // Short of the limit the same module finishes.
        let data = synthetic_data(20);
        let reference = run(&program, &data, &options);
        let chart = run_chart(&module, &program, &data, &options);
        assert_eq!(
            assert_agrees(&reference, &chart, source),
            Agreement::Finished
        );
    }
}

/// A carried offset is a float, and the interpreter reads a float offset
/// below 2^64 as that many bars. One of 2^63 or more does not fit the cell
/// the module reads its offsets from. It is past every `max_bars_back`, so
/// the interpreter stops there, and the module hands the run back for it. A
/// larger or missing one reads as `na` in both.
#[test]
fn a_carried_history_offset_the_cell_cannot_hold_is_handed_back() {
    let source = |factor: &str| {
        format!(
            "//@version=6\nindicator(\"Far\", max_bars_back=20)\nn = input.int(7)\noff = n / 2 * {factor}\nplot(close[off])\nplot(off)\n"
        )
    };
    let data = synthetic_data(8);
    let options = RunOptions::default();
    // 3.5 times 4e18 is 1.4e19: between 2^63 and 2^64, and exact in f64.
    let far = source("4000000000000000000");
    let program = compile_csil(&far);
    assert!(
        pinewasm_aot::analysis::analyze(&program).is_err(),
        "carried"
    );
    let module = compile_chart_wasm_program(&program).expect("compiles");
    let reference = run(&program, &data, &options);
    assert_eq!(
        reference.errors[0].message,
        "history offset 14000000000000000000 exceeds the max_bars_back limit of 20"
    );
    let chart = run_chart(&module, &program, &data, &options);
    assert_eq!(chart.status, status::ABI_MISMATCH);
    assert_eq!(chart.err_bar, 0);
    assert_eq!(Some(chart.err_pc), reference.errors[0].instruction_offset);
    assert_eq!(
        assert_agrees(&reference, &chart, &far),
        Agreement::HandedBack
    );

    // An offset of 2^64 or more is no offset to the interpreter either, and
    // neither is a negative one: both read `na`. Zero reads the bar itself.
    // All three finish, alike.
    for factor in ["8000000000000000000", "(0 - 2)", "0"] {
        let near = source(factor);
        let program = compile_csil(&near);
        let module = compile_chart_wasm_program(&program).expect("compiles");
        let reference = run(&program, &data, &options);
        let chart = run_chart(&module, &program, &data, &options);
        assert_eq!(
            assert_agrees(&reference, &chart, &near),
            Agreement::Finished,
            "{near}"
        );
    }
}

/// The largest string budget a linked module can be given is the largest
/// number its host counts bytes in, and that one is no budget: the
/// interpreter's sum saturates there. The module is told it has the largest
/// number left, and runs as under any budget it does not spend.
#[test]
fn the_largest_string_budget_is_no_budget() {
    let source = "//@version=6\nindicator(\"Unlimited\")\nside = bar_index % 2 == 0 ? \"even\" : \"odd\"\nalertcondition(close > open, \"Up\", side)\nplot(side == \"even\" ? 1 : 0)\n";
    let program = compile_csil(source);
    let module = compile_chart_wasm_program(&program).expect("compiles");
    let data = synthetic_data(24);
    let seen = sweep(
        &module,
        &program,
        &data,
        [u32::MAX as usize, u32::MAX as usize - 1].map(|budget| RunOptions {
            max_string_bytes_per_run: budget,
            ..RunOptions::default()
        }),
        source,
    );
    assert_eq!(seen.finished, 2, "{seen:?}");
}

/// Even a run without hosted instructions charges each native string push.
#[test]
fn a_native_string_constant_respects_the_interpreters_budget() {
    let program = compile_csil(
        "//@version=6\nindicator(\"String budget\")\ns = input.string(\"alpha\")\nplot(s == \"beta\" ? close : open)\n",
    );
    let module = compile_chart_wasm_program(&program).expect("compiles");
    assert!(module.layout.host_sites.is_empty());
    let data = synthetic_data(2);
    let options = RunOptions {
        max_string_bytes_per_run: 0,
        ..RunOptions::default()
    };
    let reference = run(&program, &data, &options);
    let error = reference.errors.first().expect("the interpreter stops");
    assert_eq!(
        error.kind,
        pinewasm_vm::RuntimeErrorKind::StringLimitExceeded
    );
    let chart = run_chart(&module, &program, &data, &options);
    assert_eq!(
        chart.status, 5,
        "the linked module stops with its host error"
    );
    assert_eq!(chart.error, wire!(error));
}

/// The module's memory layout is fixed by one `prepare`, and its linked
/// host allocates above it, so the entry points hold a loader to the order:
/// `prepare` once, then `chartInit` once, then `run` once.
#[test]
fn the_entry_points_enforce_the_loaders_order() {
    let program = compile_csil(
        "//@version=6\nindicator(\"Order\")\ns = input.string(\"a\")\nplot(s == \"a\" ? close : open, color = close > open ? color.green : color.red)\n",
    );
    let module = compile_chart_wasm_program(&program).expect("compiles");
    assert!(
        module.layout.host_sites.is_empty(),
        "this module runs no instruction on its host"
    );
    let data = synthetic_data(8);
    let options = RunOptions::default();

    // Nothing but `prepare` does anything before `prepare`.
    let mut early = Chart::new(&module);
    assert_eq!(early.alloc(64), 0, "chartAlloc before prepare");
    assert_eq!(early.command_raw(command::DRAIN_LOGS, 0, 0, 0), 0);
    assert_eq!(early.run(), status::ABI_MISMATCH, "run before prepare");

    // `run` without `chartInit` does not run: the module would read string
    // input cells no host wrote.
    let mut uninitialized = Chart::new(&module);
    assert_eq!(uninitialized.prepare(8), status::OK);
    uninitialized.load(&module, &program, &data, &options, 8);
    assert_eq!(uninitialized.run(), status::ABI_MISMATCH);
    assert_eq!(
        u32_at(uninitialized.memory(), header::OFF_ERR_CODE) as i32,
        status::ABI_MISMATCH
    );

    let mut chart = Chart::new(&module);
    assert_eq!(chart.prepare(8), status::OK);
    assert_eq!(chart.prepare(8), status::ABI_MISMATCH, "a second prepare");
    assert_eq!(chart.prepare(9), status::ABI_MISMATCH, "a larger prepare");
    chart.load(&module, &program, &data, &options, 8);
    chart
        .init(&request(&data, &options, 8))
        .expect("first chartInit");
    assert_eq!(
        chart.init(&request(&data, &options, 8)).unwrap_err(),
        "a chart instance runs once"
    );
    assert_eq!(chart.run(), status::OK);
    assert_eq!(chart.run(), status::ABI_MISMATCH, "a second run");
    // The refused run changed nothing the loader reads the first run by.
    assert_eq!(
        u32_at(chart.memory(), header::OFF_ERR_CODE) as i32,
        status::OK
    );
    let lane = chart
        .command(command::COLOR_SERIES, 0, 0, 0)
        .expect("the first run's color lane");
    assert_eq!(lane["index"].as_array().map(Vec::len), Some(8));
}

/// `chartInit` reads its request from an address and a length the loader
/// names. Neither may reach outside the module's memory, and the scratch
/// cells are where `prepare` put them, not where a later header says.
#[test]
fn chart_init_declines_what_lies_outside_the_modules_memory() {
    let program = compile_csil(
        "//@version=6\nindicator(\"Bounds\")\nlabel.new(bar_index, close, str.tostring(close))\nplot(close)\n",
    );
    let module = compile_chart_wasm_program(&program).expect("compiles");
    let data = synthetic_data(8);
    let options = RunOptions::default();
    let outside = "a chart request lies outside the module's memory";

    for (at, len) in [(0_i32, 16_i32), (-16, 64), (i32::MAX, 1), (4096, -1)] {
        let mut chart = Chart::new(&module);
        assert_eq!(chart.prepare(8), status::OK);
        let status = chart
            .instance
            .get_typed_func::<(i32, i32), i32>(&chart.store, "chartInit")
            .unwrap()
            .call(&mut chart.store, (at, len))
            .expect("chartInit does not trap");
        assert_eq!(status, status::ABI_MISMATCH, "request at {at}, {len} bytes");
        assert_eq!(
            chart.command(command::INIT_ERROR, 0, 0, 0),
            Ok(Json::String(outside.into())),
            "request at {at}, {len} bytes"
        );
        assert_eq!(chart.run(), status::ABI_MISMATCH);
    }

    // A header rewritten after `prepare` cannot move the scratch cells the
    // host writes through.
    for base in [0_u32, 0xFFFF_FFF8, 12] {
        let mut chart = Chart::new(&module);
        assert_eq!(chart.prepare(8), status::OK);
        chart.load(&module, &program, &data, &options, 8);
        let at = header::OFF_SCRATCH_BASE as usize;
        chart.memory.data_mut(&mut chart.store)[at..at + 4].copy_from_slice(&base.to_le_bytes());
        assert_eq!(
            chart.init(&request(&data, &options, 8)).unwrap_err(),
            "the chart request differs from the prepared layout",
            "scratch base {base:#x}"
        );
    }
}

/// A program the typed pass lowers only after the fraction carry rewrote it
/// still runs its host sites on the program as written, which is the one the
/// linked host holds.
#[test]
fn a_carried_program_serves_its_host_sites_from_the_program_as_written() {
    for body in [
        "half = bar_index / 2 + 1\nif bar_index % 5 == 0\n    log.info(str.tostring(close))\nplot(half)\n",
        "var int acc = 0\nacc := acc + n / 2\ns = str.tostring(volume) + \"x\"\nplot(acc + str.length(s))\n",
        "k = n / 2 + 1\nvar label l = na\nif barstate.islast\n    l := label.new(bar_index, high, \"last\")\nplot(k)\n",
        "x = bar_index * 3\nk = x / 2 + x\nif bar_index % 5 == 0\n    log.info(str.tostring(x) + str.tostring(n))\nplot(k)\n",
    ] {
        let source = format!(
            "//@version=6\nindicator(\"kept quotient\")\nn = input.int(7, \"N\")\n{body}"
        );
        let program = compile_csil(&source);
        assert!(
            pinewasm_aot::analysis::analyze(&program).is_err(),
            "{body}: the fixture must need the carry"
        );
        let module = assert_source_matches(&source, &RunOptions::default(), &[0, 1, 24]);
        assert!(!module.layout.host_sites.is_empty(), "{body}: a host site");
        for site in &module.layout.host_sites {
            assert!(
                (site.pc as usize) < program.main().code.len(),
                "{body}: site pc {} is outside the program as written",
                site.pc
            );
        }
    }
}

/// `chartInit` declines a request that does not fit the prepared module,
/// with a reason, and the run then reroutes.
#[test]
fn chart_init_declines_a_request_that_does_not_fit() {
    let program =
        compile_csil("//@version=6\nstrategy(\"Fit\")\nif bar_index == 2\n    strategy.entry(\"L\", strategy.long)\nplot(close)\n");
    let module = compile_chart_wasm_program(&program).expect("compiles");
    let data = synthetic_data(8);
    let options = RunOptions::default();

    let mut wrong_bars = Chart::new(&module);
    assert_eq!(wrong_bars.prepare(8), status::OK);
    assert_eq!(
        wrong_bars.init(&request(&data, &options, 7)).unwrap_err(),
        "the chart request differs from the prepared layout"
    );
    assert_eq!(wrong_bars.run(), status::ABI_MISMATCH);

    let mut malformed = Chart::new(&module);
    assert_eq!(malformed.prepare(8), status::OK);
    assert!(malformed.init(&json!({ "bars": 8 })).is_err());

    // A strategy setting the native bar loop cannot follow is the host's to
    // refuse, before the run.
    let mut recalculating = RunOptions::default();
    recalculating.strategy.calc_on_order_fills = Some(true);
    let mut refused = Chart::new(&module);
    assert_eq!(refused.prepare(8), status::OK);
    assert_eq!(
        refused
            .init(&request(&data, &recalculating, 8))
            .unwrap_err(),
        "calc_on_order_fills recalculates the script after fills"
    );
}

/// The runtime's header constants are the compiler's.
#[test]
fn the_runtime_reads_the_header_the_compiler_writes() {
    let chart = include_str!("../runtime/src/chart.rs");
    for (name, value) in [
        ("OFF_ABI_VERSION", header::OFF_ABI_VERSION),
        ("OFF_BARS", header::OFF_BARS),
        ("OFF_SLOTS_BASE", header::OFF_SLOTS_BASE),
        ("OFF_ERR_CODE", header::OFF_ERR_CODE),
        ("OFF_ERR_BAR", header::OFF_ERR_BAR),
        ("OFF_COLORS_BASE", header::OFF_COLORS_BASE),
        ("OFF_SCRATCH_BASE", header::OFF_SCRATCH_BASE),
        ("OFF_ALERTS_BASE", header::OFF_ALERTS_BASE),
        ("OFF_ALERT_COUNT", header::OFF_ALERT_COUNT),
        ("INPUT_TABLE_BASE", header::SIZE),
    ] {
        assert!(
            chart.contains(&format!("const {name}: u32 = {value:#04X};"))
                || chart.contains(&format!("const {name}: u32 = {value:#04x};")),
            "{name} = {value:#x}"
        );
    }
    assert!(chart.contains(&format!(
        "const ABI_VERSION: u32 = {};",
        header::ABI_VERSION
    )));
    assert!(chart.contains(&format!(
        "const STATUS_ABI_MISMATCH: i32 = {};",
        status::ABI_MISMATCH
    )));
    assert!(chart.contains(&format!(
        "const ALERT_ENTRY: u32 = {};",
        header::ALERT_ENTRY
    )));
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

/// Every eligible fixture compiles as a linked chart module that imports
/// nothing, and its run equals the interpreter's. `drawings.pine` is declined
/// before emission because its non-var table constructor repeats across bars;
/// the linked module cannot rewrite the aliases that position replacement
/// invalidates. The counts pin that capability boundary.
#[test]
fn every_public_fixture_matches_the_interpreter() {
    let mut files = Vec::new();
    collect_pine_files(Path::new("../../tests/compile-pass"), &mut files);
    files.sort();
    let (mut linked, mut refused, mut rerouted) = (0usize, Vec::new(), Vec::new());
    // Runs compared with the interpreter, and runs skipped because the
    // interpreter itself fails on this feed.
    let (mut compared, mut failing) = (0usize, 0usize);
    for path in &files {
        let source = std::fs::read_to_string(path).expect("fixture");
        let result = compile(&source, CompileOptions::default());
        let Some(program) = result.program else {
            continue;
        };
        if result
            .diagnostics
            .iter()
            .any(pinewasm_span::Diagnostic::is_error)
        {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let module = match compile_chart_wasm_program(&program) {
            Ok(module) => module,
            Err(error) => {
                refused.push(format!("{name}: {error}"));
                continue;
            }
        };
        linked += 1;
        assert_eq!(imports_of(&module.wasm), Vec::<String>::new(), "{name}");
        for bars in [0, 1, 48] {
            let data = synthetic_data(bars);
            let options = RunOptions::default();
            let reference = run(&program, &data, &options);
            if !reference.errors.is_empty() {
                // The fixture fails on this feed; a failing run is covered
                // by its own cases above.
                failing += 1;
                continue;
            }
            let chart = run_chart(&module, &program, &data, &options);
            if chart.status == status::ABI_MISMATCH && !chart.mismatch.is_null() {
                if !rerouted.contains(&name) {
                    eprintln!("[chart summary] {name} reroutes: {}", chart.mismatch);
                    rerouted.push(name.clone());
                }
                continue;
            }
            assert_run_matches(&reference, &chart, bars, &format!("{name}, {bars} bars"));
            compared += 1;
        }
    }
    eprintln!(
        "[chart summary] linked={linked} refused={} rerouted={} compared={compared} failing={failing}",
        refused.len(),
        rerouted.len()
    );
    for line in &refused {
        eprintln!("[chart summary]   refused {line}");
    }
    assert_eq!(
        (linked, refused, rerouted, compared, failing),
        (
            50,
            vec!["drawings.pine: unsupported opcode for AOT: table position replacement requires interpreter".to_string()],
            Vec::new(),
            150,
            0,
        ),
        "the linked chart fixture counts changed; review the boundary and update this assertion"
    );
}

#[test]
fn linked_chart_memory_has_a_finite_graphics_allowance() {
    let program = compile_csil("//@version=6\nindicator(\"Memory cap\")\nplot(close)\n");
    let artifact = compile_chart_wasm_program(&program).unwrap();
    let mut memories = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(&artifact.wasm) {
        if let wasmparser::Payload::MemorySection(section) = payload.unwrap() {
            memories.extend(section.into_iter().map(|memory| memory.unwrap()));
        }
    }
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].maximum, Some(16_384));
    assert!(!memories[0].memory64 && !memories[0].shared);
}

#[test]
fn indexed_ordered_scan_is_native_aot_and_matches_vm_for_sources_and_neighbors() {
    let source = include_str!("../../../tests/regression/indexed_distance_scan.pine");
    let program = compile_csil(source);
    assert_eq!(
        program.header.runtime_abi_version,
        pinewasm_csil::INDEXED_SCAN_ABI_VERSION
    );
    let module =
        compile_chart_wasm_program(&program).expect("the guarded scan compiles to native AOT");
    assert_eq!(imports_of(&module.wasm), Vec::<String>::new());
    assert_eq!(
        module.layout.execution_mode,
        pinewasm_aot::ExecutionMode::Native
    );
    for k in [1, 2, 8] {
        for f in [2, 5] {
            for high in [false, true] {
                let mut options = RunOptions::default();
                options.inputs.insert(0, InputValue::Int(k));
                options.inputs.insert(1, InputValue::Int(f));
                if high {
                    options
                        .inputs
                        .insert(4, InputValue::Source(pinewasm_hir::BuiltinSeries::High));
                }
                let data = synthetic_data(129);
                let reference = run(&program, &data, &options);
                let chart = run_chart(&module, &program, &data, &options);
                assert_run_matches(&reference, &chart, 129, "indexed scan");
            }
        }
    }
}

#[test]
fn indexed_ordered_scan_budget_stop_is_the_vms_or_requests_exact_replay() {
    let source = include_str!("../../../tests/regression/indexed_distance_scan.pine");
    let program = compile_csil(source);
    let module = compile_chart_wasm_program(&program).expect("compiles");
    let data = synthetic_data(129);
    for (bar_budget, run_budget) in [(2_000, 1_000_000_000), (64_000_000, 100_000)] {
        let options = RunOptions {
            max_instructions_per_bar: bar_budget,
            max_instructions_per_run: run_budget,
            ..RunOptions::default()
        };
        let reference = run(&program, &data, &options);
        let error = reference.errors.first().expect("tight budget must stop");
        assert!(matches!(
            error.kind,
            pinewasm_vm::RuntimeErrorKind::InstructionLimitExceeded
                | pinewasm_vm::RuntimeErrorKind::RunInstructionLimitExceeded
        ));
        let chart = run_chart(&module, &program, &data, &options);
        assert_agrees(&reference, &chart, "indexed scan budget stop");
    }
}

#[test]
fn indexed_scan_guard_keeps_fractional_int_operands_on_the_literal_path() {
    let source = include_str!("../../../tests/regression/indexed_distance_scan.pine");
    for source in [
        source.replace("start = input.int(0, \"Start\")", "start = 1 / 2"),
        source.replace("k = input.int(8, \"Neighbors\")", "k = 7 / 2"),
        source.replace("f = input.int(5, \"Features\")", "f = 7 / 2"),
    ] {
        let program = compile_csil(&source);
        match compile_chart_wasm_program(&program) {
            Ok(_) => {
                assert_source_matches(&source, &RunOptions::default(), &[129]);
            }
            // The numeric backend deliberately refuses fractional integer
            // flow into an opaque operation. The universal route must use
            // the VM, whose literal fallback is checked by the compiler test.
            Err(AotError::UnsupportedOpcode(reason)) => assert_eq!(reason, "PineIntDiv"),
            Err(error) => panic!("unexpected fractional-guard failure: {error}"),
        }
    }
}

#[test]
fn linked_gradient_fills_preserve_numeric_boundaries_and_colors() {
    for bounds in [
        "60.0, 0.0",
        "close + 0.125, open - 0.375",
        "bar_index == 0 ? na : close, -0.0",
    ] {
        let source = format!("//@version=6\nindicator(\"Gradient\")\ntint=input.color(color.blue)\na=plot(close)\nb=plot(open)\nfill(a,b,{bounds},color.new(tint,12.5),color.new(color.gray,77.5))\n");
        assert_source_matches(&source, &RunOptions::default(), &[0, 1, 17]);
    }
}

#[test]
fn indexed_scan_rebuilds_keep_the_linked_host_memory_reservation_bounded() {
    let source = include_str!("../../../tests/regression/indexed_distance_scan.pine")
        .replace("featureArrays = FeatureArrays.new", "if bar_index > 64\n    array.set(values_1, 1, 0.2 + 0.1 * math.sin(bar_index))\nfeatureArrays = FeatureArrays.new") + "\nlog.info(\"tick\")\n";
    let program = compile_csil(&source);
    let module = compile_chart_wasm_program(&program).unwrap();
    assert_eq!(module.layout.distance_scan_kernels, 1);
    assert!(imports_of(&module.wasm).is_empty());
    let options = RunOptions {
        max_vm_bytes_per_run: 2_000_000,
        max_instructions_per_run: 10_000_000_000,
        ..Default::default()
    };
    let data = synthetic_data(1025);
    let reference = run(&program, &data, &options);
    assert!(reference.errors.is_empty(), "{:?}", reference.errors);
    let chart = run_chart(&module, &program, &data, &options);
    assert_run_matches(&reference, &chart, 1025, "bounded linked index rebuilds");
}

// These are local runtime regression tests.
// Each regression input is an inline source pinned by its hash, so a changed
// input fails the test.
// The four-bar expected vectors are hand-calculated from the semantics of
// history access and expression-site timing.
// The enclosing linked chart suite runs under the pinewasm_chart_runtime cfg;
// a native refusal or reroute must fail these tests.
mod computed_history_regressions {
    use super::*;
    use sha2::{Digest, Sha256};

    struct HistoryCase {
        id: &'static str,
        source: &'static str,
        sha256: &'static str,
        expected: &'static [&'static [Option<f64>]],
    }

    fn market() -> MarketData {
        MarketData {
            time: vec![
                1_700_000_000_000,
                1_700_000_060_000,
                1_700_000_120_000,
                1_700_000_180_000,
            ],
            open: vec![1.0, 2.0, 1.0, 2.0],
            high: vec![3.0; 4],
            low: vec![0.0; 4],
            close: vec![2.0, 1.0, 2.0, 1.0],
            volume: vec![1.0; 4],
            mintick: 0.01,
            ..MarketData::default()
        }
    }

    fn source(body: &str) -> String {
        format!("//@version=6\nindicator(\"Computed history safeguard\")\n{body}\n")
    }

    fn native(source: &str, sha256: &str) -> (CsilProgram, AotModule) {
        assert_eq!(
            Sha256::digest(source.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            sha256,
            "source bytes changed"
        );
        let program = compile_csil(source);
        let module = compile_chart_wasm_program(&program)
            .expect("computed history must compile to a linked chart module");
        assert_eq!(
            module.layout.execution_mode,
            pinewasm_aot::ExecutionMode::Native,
            "computed history must execute the native bar loop"
        );
        assert!(
            imports_of(&module.wasm).is_empty(),
            "the linked module needs no external host"
        );
        (program, module)
    }

    fn assert_values(actual: &[f64], expected: &[Option<f64>], context: &str) {
        assert_eq!(actual.len(), expected.len(), "{context}: lane length");
        for (bar, (actual, expected)) in actual.iter().zip(expected).enumerate() {
            match expected {
                None => assert!(
                    actual.is_nan(),
                    "{context} bar {bar}: {actual} must be missing"
                ),
                Some(expected) => assert_eq!(
                    actual.to_bits(),
                    expected.to_bits(),
                    "{context} bar {bar}: {actual} != {expected}"
                ),
            }
        }
    }

    fn assert_finished(
        program: &CsilProgram,
        module: &AotModule,
        options: &RunOptions,
        expected: &[&[Option<f64>]],
        context: &str,
    ) {
        let data = market();
        let reference = run(program, &data, options);
        assert_eq!(
            reference.bars_executed, 4,
            "{context}: interpreter bar count"
        );
        let chart = run_chart(module, program, &data, options);
        assert_run_matches(&reference, &chart, 4, context);
        assert_eq!(chart.mismatch, Json::Null, "{context}: no fallback reason");
        assert_eq!(chart.plots.len(), expected.len(), "{context}: plot count");
        // Differential equality includes all plot/color/style/other outputs.
        // Check both routes against separate hand-calculated vectors as well,
        // so agreement on a shared incorrect history implementation cannot pass.
        for (slot, values) in expected.iter().enumerate() {
            let reference_values: Vec<f64> = reference.plots[slot]
                .values
                .iter()
                .map(|value| match value {
                    Value::Na => f64::NAN,
                    Value::Float(value) => *value,
                    other => panic!("{context}: unexpected numeric plot value {other:?}"),
                })
                .collect();
            assert_values(
                &reference_values,
                values,
                &format!("{context}, interpreter plot {slot}"),
            );
            assert_values(
                &chart.plots[slot],
                values,
                &format!("{context}, native plot {slot}"),
            );
        }
    }

    fn assert_default(case: &HistoryCase) {
        let (program, module) = native(case.source, case.sha256);
        assert_finished(
            &program,
            &module,
            &RunOptions::default(),
            case.expected,
            case.id,
        );
    }

    const NUMERIC_TARGETS: &[HistoryCase] = &[
        HistoryCase {
            id: "persistent-compound-history/var_float_compound_history",
            source: r#"//@version=6
indicator("Persistent compound history discriminator")
var float k = 1.0
k := 2.0
plot((k * 2)[1])
"#,
            sha256: "5e225d5f0d8e0d49bf1d6077f4c86bd1400ea09dadff0fe353c1f987e477be3f",
            expected: &[&[None, Some(4.0), Some(4.0), Some(4.0)]],
        },
        HistoryCase {
            id: "persistent-compound-history/varip_float_compound_history",
            source: r#"//@version=6
indicator("Persistent compound history discriminator")
varip float k = 1.0
k := 2.0
plot((k * 2)[1])
"#,
            sha256: "738efbe98f79241089141c06ee9e674c40bfe6e461a26e63d7fca9cbe9a51fe1",
            expected: &[&[None, Some(4.0), Some(4.0), Some(4.0)]],
        },
        HistoryCase {
            id: "persistent-compound-history/var_float_direct_history",
            source: r#"//@version=6
indicator("Persistent compound history discriminator")
var float k = 1.0
k := 2.0
plot(k[1])
"#,
            sha256: "6fbed7d3e37cda5821aaaf521ec67d4368d48d6eff077f3b022970cd3370a0d8",
            expected: &[&[None, Some(2.0), Some(2.0), Some(2.0)]],
        },
        HistoryCase {
            id: "persistent-compound-history/ordinary_float_compound_history",
            source: r#"//@version=6
indicator("Persistent compound history discriminator")
float k = 1.0
k := 2.0
plot((k * 2)[1])
"#,
            sha256: "df7dc0d974e9a68b09ffe90b4fa87b3f1445d534bb7fe15ca6e3d87f76cb75c1",
            expected: &[&[None, Some(4.0), Some(4.0), Some(4.0)]],
        },
        HistoryCase {
            id: "persistent-compound-history/constant_compound_history",
            source: r#"//@version=6
indicator("Persistent compound history discriminator")
plot((1.0 * 2)[1])
"#,
            sha256: "f3b2f3477b324207d8f15b51eae71304f59e1765f6ec355231347eeca5e7ea6f",
            expected: &[&[None, Some(2.0), Some(2.0), Some(2.0)]],
        },
        HistoryCase {
            id: "persistent-compound-history/var_float_compound_input_offset",
            source: r#"//@version=6
indicator("Persistent compound history discriminator")
int lag = input.int(1)
var float k = 1.0
k := 2.0
plot((k * 2)[lag])
"#,
            sha256: "d44d833de9aae21d3780cb70be04c257d1c951b424e563e5ecd1f0844fe83926",
            expected: &[&[None, Some(4.0), Some(4.0), Some(4.0)]],
        },
        HistoryCase {
            id: "compound-history-boundary/const_identifier_history",
            source: r#"//@version=6
indicator("Compound history syntax boundary")
const float k = 1.0
plot(k[1])
"#,
            sha256: "618fbe44df2c826c946fcbb8f6780f534e8e22ba9cd9577e6a0a6fe1f631c879",
            expected: &[&[None, Some(1.0), Some(1.0), Some(1.0)]],
        },
        HistoryCase {
            id: "compound-history-boundary/input_scalar_compound_history",
            source: r#"//@version=6
indicator("Compound history syntax boundary")
float k = input.float(1.0)
plot((k * 2)[1])
"#,
            sha256: "6f45737cfa51349f1e29fb53fc8683e1b3ffcd67c4aa8723b95050c9dfabe18a",
            expected: &[&[None, Some(2.0), Some(2.0), Some(2.0)]],
        },
        HistoryCase {
            id: "compound-history-boundary/genuine_simple_compound_history",
            source: r#"//@version=6
indicator("Compound history syntax boundary")
plot((syminfo.mintick * 2)[1])
"#,
            sha256: "6bf7ba6f77ca064c0f077b2d61b2df21f9b8e4106850bc614953bdc370af7ecc",
            expected: &[&[None, Some(0.02), Some(0.02), Some(0.02)]],
        },
        HistoryCase {
            id: "compound-history-targets/negative_numeric_unary_history",
            source: r#"//@version=6
indicator("History target shape boundary")
plot((-2.0)[1])
"#,
            sha256: "36662a87d6fd522efc0356081a3e45d9a90393ff3d0af7453df2df461be5635c",
            expected: &[&[None, Some(-2.0), Some(-2.0), Some(-2.0)]],
        },
        HistoryCase {
            id: "compound-history-targets/pure_float_call_history",
            source: r#"//@version=6
indicator("History target shape boundary")
plot(float(2)[1])
"#,
            sha256: "eadb769abeab7a143de38f529c90074ee543ad428aafcf43fa602f53fbc6032d",
            expected: &[&[None, Some(2.0), Some(2.0), Some(2.0)]],
        },
        HistoryCase {
            id: "compound-history-targets/bare_genuine_simple_history",
            source: r#"//@version=6
indicator("History target shape boundary")
plot(syminfo.mintick[1])
"#,
            sha256: "1d6a0c45552d039a9bd9a5d2f52b7e157d3791b12d35930ff565610ebe4400c2",
            expected: &[&[None, Some(0.01), Some(0.01), Some(0.01)]],
        },
    ];

    const LEADING_BOOL_AND_COLOR: &[HistoryCase] = &[
        HistoryCase {
            id: "compound-history-boundary/constant_bool_compound_history",
            source: r#"//@version=6
indicator("Compound history syntax boundary")
plot((true and true)[1] ? 1 : 0)
"#,
            sha256: "837736cc1df919230e6f9653843571ba5c4f88e26fe59bd6b335df44cad67511",
            expected: &[&[Some(0.0), Some(1.0), Some(1.0), Some(1.0)]],
        },
        HistoryCase {
            id: "compound-history-targets/computed_const_color_history",
            source: r#"//@version=6
indicator("History target shape boundary")
color c = color.new(color.red, 0)[1]
plot(c == color.red ? 1 : 0)
"#,
            sha256: "dea20f2ed24d55fbeb3a3070aa0e7091605c2b4435980d2896089fecc6c7770f",
            expected: &[&[Some(0.0), Some(1.0), Some(1.0), Some(1.0)]],
        },
    ];

    #[test]
    fn computed_const_input_simple_and_persistent_targets_execute_natively() {
        // Includes the var and varip cases, static and Input
        // offsets, ordinary writes, Const bindings and genuine Simple context.
        for case in NUMERIC_TARGETS {
            assert_default(case);
        }
    }

    #[test]
    fn computed_bool_and_color_history_keep_their_leading_values_natively() {
        for case in LEADING_BOOL_AND_COLOR {
            assert_default(case);
        }
        let text = source(
            "flag = input.bool(true)\nlag = input.int(1)\nplot((flag and true)[lag] ? 1 : 0)",
        );
        let (program, module) = native(
            &text,
            "3260be484b0a138712b590bcbe1716a078d9a1c57476246bb8290f0db371245b",
        );
        assert_eq!(
            program
                .inputs
                .iter()
                .map(|input| input.id)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        for (flag, lag, expected) in [
            (true, 0, [Some(1.0); 4]),
            (true, 1, [Some(0.0), Some(1.0), Some(1.0), Some(1.0)]),
            (true, 2, [Some(0.0), Some(0.0), Some(1.0), Some(1.0)]),
            (false, 1, [Some(0.0); 4]),
        ] {
            let mut options = RunOptions::default();
            options.inputs.insert(0, InputValue::Bool(flag));
            options.inputs.insert(1, InputValue::Int(lag));
            assert_finished(
                &program,
                &module,
                &options,
                &[&expected],
                "Bool history overrides",
            );
        }
    }

    #[test]
    fn folded_readvar_history_snapshots_the_site_before_later_writes_natively() {
        // The computed site (+k)[lag] snapshots k before the later write, so it
        // must keep 2; a direct k history reads 3.
        for (target, static_sha, dynamic_sha) in [
            (
                "+k",
                "b96b4c6bcd1674d4ed8f5ad9d5ea3693612d94885a123d5ac1b921d752b359a6",
                "05a2ee662f71973ce6d1e519a2ae34fa2a1b9c4299ac1e46bdfc4a01bece8f86",
            ),
            (
                "float(k)",
                "9cd83d00affe2dd25f4dcb33da8f719c6bb776ad0be21909860fd3f6e264408d",
                "28dc122bfa9a2f2a562efff0b59bf3b1267bb7427b475eb24edda198cfe3eedd",
            ),
            (
                "identity(k)",
                "67b45396f8529a2e0f779ee4d430a8150c092c692e3a432c0a8d38c6b61baf6e",
                "8a6fe23e05e5180316e625d7927b6ddf458ffdea610c28145d53fcb549fefdfe",
            ),
        ] {
            for (offset, sha256) in [("1", static_sha), ("lag", dynamic_sha)] {
                let text = source(&format!(
                    "identity(float value) => value\n\
                     float k = 2.0\n\
                     int lag = input.int(1)\n\
                     held = ({target})[{offset}]\n\
                     k := 3.0\n\
                     plot(held)\n\
                     plot(k[{offset}])"
                ));
                let (program, module) = native(&text, sha256);
                assert_finished(
                    &program,
                    &module,
                    &RunOptions::default(),
                    &[
                        &[None, Some(2.0), Some(2.0), Some(2.0)],
                        &[None, Some(3.0), Some(3.0), Some(3.0)],
                    ],
                    "computed read site versus final variable slot",
                );
                if offset == "lag" {
                    let mut options = RunOptions::default();
                    options.inputs.insert(0, InputValue::Int(0));
                    assert_finished(
                        &program,
                        &module,
                        &options,
                        &[&[Some(2.0); 4], &[Some(3.0); 4]],
                        "zero offset snapshots the site",
                    );
                }
            }
        }
    }

    #[test]
    fn computed_history_value_and_offset_overrides_bind_per_native_run() {
        let text = source(
            "simple float value = input.float(1.0, \"Value\")\n\
                           int lag = input.int(1, \"Lag\")\n\
                           plot((value * 2)[lag])",
        );
        let (program, module) = native(
            &text,
            "ad0ffea1986fb35c6014ea0e7dec914604012cb7359906acc39fe7543cf33072",
        );
        assert_eq!(
            program
                .inputs
                .iter()
                .map(|input| input.id)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        for (lag, expected) in [
            (0, [Some(7.0); 4]),
            (1, [None, Some(7.0), Some(7.0), Some(7.0)]),
            (2, [None, None, Some(7.0), Some(7.0)]),
            (-1, [None; 4]),
        ] {
            let mut options = RunOptions::default();
            options.inputs.insert(0, InputValue::Float(3.5));
            options.inputs.insert(1, InputValue::Int(lag));
            assert_finished(
                &program,
                &module,
                &options,
                &[&expected],
                "value and lag overrides",
            );
        }
        assert_finished(
            &program,
            &module,
            &RunOptions::default(),
            &[&[None, Some(2.0), Some(2.0), Some(2.0)]],
            "defaults after overrides",
        );
    }

    #[test]
    fn computed_history_materializes_effects_once_in_source_order_natively() {
        let cases = [
            HistoryCase {
                id: "once-target-1",
                source: r#"//@version=6
indicator("Computed history safeguard")
var calls = array.new_int()
tap() =>
    array.push(calls, 1)
    2.0
lag = input.int(1)
plot(tap()[1])
plot(array.size(calls))
"#,
                sha256: "7d09d787c97cefd648a642c0938f544c0b58cd394a59171392b6e1a7acf27637",
                expected: &[
                    &[None, Some(2.0), Some(2.0), Some(2.0)],
                    &[Some(1.0), Some(2.0), Some(3.0), Some(4.0)],
                ],
            },
            HistoryCase {
                id: "once-target-lag",
                source: r#"//@version=6
indicator("Computed history safeguard")
var calls = array.new_int()
tap() =>
    array.push(calls, 1)
    2.0
lag = input.int(1)
plot(tap()[lag])
plot(array.size(calls))
"#,
                sha256: "76f3091de94a8f0736d0922cd489fd6ff62dab9deeb37b17e04b707d60a1f4d5",
                expected: &[
                    &[None, Some(2.0), Some(2.0), Some(2.0)],
                    &[Some(1.0), Some(2.0), Some(3.0), Some(4.0)],
                ],
            },
            HistoryCase {
                id: "once-offset",
                source: r#"//@version=6
indicator("Computed history safeguard")
var calls = array.new_int()
lag() =>
    array.push(calls, 1)
    1
plot((1 + 2)[lag()])
plot(array.size(calls))
"#,
                sha256: "573efc99534ed0f89866bfd4ceebb3e97a070310d98e9921a4a544c4b6bee230",
                expected: &[
                    &[None, Some(3.0), Some(3.0), Some(3.0)],
                    &[Some(1.0), Some(2.0), Some(3.0), Some(4.0)],
                ],
            },
            HistoryCase {
                id: "inline-target-offset-pop-order",
                source: r#"//@version=6
indicator("Computed history safeguard")
values = array.from(1.0, 2.0)
plot((array.pop(values) + 0.0)[int(array.pop(values))])
plot(array.size(values))
"#,
                sha256: "5eb476c96c0f9238de3e1cdd424923359c707b192a56af9283551bc7dd06c537",
                expected: &[
                    &[None, Some(2.0), Some(2.0), Some(2.0)],
                    &[Some(0.0), Some(0.0), Some(0.0), Some(0.0)],
                ],
            },
        ];
        // Counters prove each target/offset call executes once per bar. The
        // pop case proves target 2 is removed before offset 1, leaving no values.
        for case in &cases {
            assert_default(case);
        }
    }

    #[test]
    fn computed_history_keeps_skipped_site_gaps_in_native_execution() {
        let cases = [
            HistoryCase {
                id: "weak-branch-gaps-1",
                source: r#"//@version=6
indicator("Computed history safeguard")
lag = input.int(1)
float a = 0.0
float b = 0.0
if bar_index % 2 == 0
    a := (1 + 2)[1]
    b := (1 + 2)[2]
plot(a)
plot(b)
"#,
                sha256: "78dc3471a91e6abc7e933863505587c2a310b8707b6b1a5804394496cb5e3d45",
                expected: &[
                    &[None, Some(0.0), None, Some(0.0)],
                    &[None, Some(0.0), Some(3.0), Some(0.0)],
                ],
            },
            HistoryCase {
                id: "weak-branch-gaps-lag",
                source: r#"//@version=6
indicator("Computed history safeguard")
lag = input.int(1)
float a = 0.0
float b = 0.0
if bar_index % 2 == 0
    a := (1 + 2)[lag]
    b := (1 + 2)[lag + 1]
plot(a)
plot(b)
"#,
                sha256: "ffed818c37547bdfe1daac799900020e65b9c9797743e2cad79ea22a90af0389",
                expected: &[
                    &[None, Some(0.0), None, Some(0.0)],
                    &[None, Some(0.0), Some(3.0), Some(0.0)],
                ],
            },
        ];
        for case in &cases {
            let result = compile(case.source, CompileOptions::default());
            assert_eq!(
                result
                    .diagnostics
                    .iter()
                    .filter(|d| d.code.as_deref() == Some("W0004"))
                    .count(),
                2,
                "a skipped materialization site must keep its scheduling warning"
            );
            assert_default(case);
        }
    }

    #[test]
    fn computed_history_var_initializer_materializes_only_once_natively() {
        let case = HistoryCase {
            id: "var-initializer-one-execution",
            source: r#"//@version=6
indicator("Computed history safeguard")
var value = (1 + 2)[1]
plot(value)
"#,
            sha256: "f8dc36e250f8af6675eb1c68ad20c05f74b5fa978810aac2c5ecc59aea2380e5",
            expected: &[&[None, None, None, None]],
        };
        let result = compile(case.source, CompileOptions::default());
        assert_eq!(
            result
                .diagnostics
                .iter()
                .filter(|d| d.code.as_deref() == Some("W0004"))
                .count(),
            1,
            "the one-time initializer keeps its scheduling warning"
        );
        assert_default(&case);
    }

    #[test]
    fn computed_history_enforces_max_bars_back_in_the_native_module() {
        let text = "//@version=6\nindicator(\"Weak dynamic history ceiling\", max_bars_back=1)\nlag = input.int(1)\nplot((1 + 2)[lag])\n";
        let (program, module) = native(
            text,
            "3b0470c8d06fc430c081ac88a5baf37ca9c2566402f7147d4773c0a3fa1dd75c",
        );
        assert_finished(
            &program,
            &module,
            &RunOptions::default(),
            &[&[None, Some(3.0), Some(3.0), Some(3.0)]],
            "within max_bars_back",
        );
        let mut options = RunOptions::default();
        options.inputs.insert(0, InputValue::Int(0));
        assert_finished(
            &program,
            &module,
            &options,
            &[&[Some(3.0); 4]],
            "zero offset within limit",
        );
        options.inputs.insert(0, InputValue::Int(2));
        let data = market();
        let reference = run(&program, &data, &options);
        assert_eq!(reference.bars_executed, 0);
        assert_eq!(reference.errors.len(), 1);
        assert_eq!(
            reference.errors[0].kind,
            pinewasm_vm::RuntimeErrorKind::HistoryBufferLimitExceeded
        );
        assert_eq!(reference.errors[0].bar_index, Some(0));
        let chart = run_chart(&module, &program, &data, &options);
        assert_eq!(
            assert_agrees(&reference, &chart, "computed history max_bars_back"),
            Agreement::Stopped,
            "the native module must stop itself; handing back is not validation"
        );
        assert_eq!(chart.mismatch, Json::Null);
    }
}

// Collection history cases, a companion to the scalar and history tests above.
// These programs also appear in the compiler regression suite, which checks
// that they compile; the four-bar array sizes are listed below.
#[test]
fn drawing_array_history_keeps_expression_site_snapshots_in_the_native_module() {
    use sha2::{Digest, Sha256};

    let data = MarketData {
        time: vec![
            1_700_000_000_000,
            1_700_000_060_000,
            1_700_000_120_000,
            1_700_000_180_000,
        ],
        open: vec![1.0, 2.0, 1.0, 2.0],
        high: vec![3.0; 4],
        low: vec![0.0; 4],
        close: vec![2.0, 1.0, 2.0, 1.0],
        volume: vec![1.0; 4],
        mintick: 0.01,
        ..MarketData::default()
    };
    // A literal offset runs once. Each dynamic source runs with the three
    // separate overrides, all against the same compiled native chart module.
    let cases = [
        (
            r#"//@version=6
indicator("Computed history safeguard")
collect() => line.all
lag = input.int(1)
line.new(bar_index, open, bar_index, close)
values = line.all[0]
plot(bar_index >= 0 ? array.size(values) : -1)
plot(array.size(line.all))
"#,
            "7a87ea22dda5a1dae56931cb34b1cf05e3dd6c166f609b568db88b87a5bcaae0",
            Some(0),
        ),
        (
            r#"//@version=6
indicator("Computed history safeguard")
collect() => line.all
lag = input.int(1)
line.new(bar_index, open, bar_index, close)
values = line.all[1]
plot(bar_index >= 1 ? array.size(values) : -1)
plot(array.size(line.all))
"#,
            "069d27d08e73552c28df8fc1f1bf6f29ed06468162b3ab4f7134f441db62cda9",
            Some(1),
        ),
        (
            r#"//@version=6
indicator("Computed history safeguard")
collect() => line.all
lag = input.int(1)
line.new(bar_index, open, bar_index, close)
values = line.all[lag]
plot(bar_index >= lag ? array.size(values) : -1)
plot(array.size(line.all))
"#,
            "3bf12b8367fbfe6a44f3b1995dce779a3c6df5bebb07996b640807b5fd733b53",
            None,
        ),
        (
            r#"//@version=6
indicator("Computed history safeguard")
collect() => line.all
lag = input.int(1)
line.new(bar_index, open, bar_index, close)
values = collect()[0]
plot(bar_index >= 0 ? array.size(values) : -1)
plot(array.size(line.all))
"#,
            "780566cdecfd5f82394d931053cbec2197259152f973cb07aaf074896b38438c",
            Some(0),
        ),
        (
            r#"//@version=6
indicator("Computed history safeguard")
collect() => line.all
lag = input.int(1)
line.new(bar_index, open, bar_index, close)
values = collect()[1]
plot(bar_index >= 1 ? array.size(values) : -1)
plot(array.size(line.all))
"#,
            "93a50044a021582daa22138fa99c7b50445ba52c520fa90c21a11ebe6b7697dc",
            Some(1),
        ),
        (
            r#"//@version=6
indicator("Computed history safeguard")
collect() => line.all
lag = input.int(1)
line.new(bar_index, open, bar_index, close)
values = collect()[lag]
plot(bar_index >= lag ? array.size(values) : -1)
plot(array.size(line.all))
"#,
            "372971def221ac68fe8371360f0168499366743ac7e982bfdbc4b59558d81e7e",
            None,
        ),
    ];
    for (source, sha256, literal_offset) in cases {
        assert_eq!(
            Sha256::digest(source.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            sha256,
            "collection history source bytes changed"
        );
        let program = compile_csil(source);
        let module = compile_chart_wasm_program(&program)
            .expect("collection history must compile to a linked chart module");
        assert_eq!(
            module.layout.execution_mode,
            pinewasm_aot::ExecutionMode::Native,
            "drawing-array history needs actual native execution"
        );
        assert!(imports_of(&module.wasm).is_empty());
        assert_eq!(program.inputs.len(), 1);
        assert_eq!(program.inputs[0].id, 0);
        for (lag, historical) in [
            (0, [1.0, 2.0, 3.0, 4.0]),
            (1, [-1.0, 1.0, 2.0, 3.0]),
            (2, [-1.0, -1.0, 1.0, 2.0]),
        ] {
            if literal_offset.is_some_and(|offset| offset != lag) {
                continue;
            }
            let mut options = RunOptions::default();
            if literal_offset.is_none() {
                options.inputs.insert(0, InputValue::Int(lag));
            }
            let reference = run(&program, &data, &options);
            assert_eq!(reference.bars_executed, 4);
            let chart = run_chart(&module, &program, &data, &options);
            let context = format!("drawing-array snapshot {sha256}, lag {lag}");
            assert_run_matches(&reference, &chart, 4, &context);
            assert_eq!(chart.mismatch, Json::Null, "no fallback credit: {context}");
            assert_eq!(chart.plots.len(), 2, "{context}");
            for (slot, expected) in [historical, [1.0, 2.0, 3.0, 4.0]].iter().enumerate() {
                assert_eq!(chart.plots[slot].len(), 4, "{context}");
                for (bar, expected) in expected.iter().enumerate() {
                    let reference_value = &reference.plots[slot].values[bar];
                    assert!(matches!(reference_value, Value::Float(actual) if actual.to_bits() == f64::to_bits(*expected)),
                        "{context}, interpreter plot {slot} bar {bar}: {reference_value:?} != {expected}");
                    assert_eq!(
                        chart.plots[slot][bar].to_bits(),
                        f64::to_bits(*expected),
                        "{context}, native plot {slot} bar {bar}"
                    );
                }
            }
        }
    }
}

// Separate missing-reference check: the guarded collection programs above
// materialize on every bar and guard only the consumer. This companion makes
// na(array) behavior explicit instead of assuming it from bar-index warmup.
#[test]
fn drawing_array_history_exposes_missing_references_to_na_in_native_execution() {
    use sha2::{Digest, Sha256};

    let source = r#"//@version=6
indicator("Computed history safeguard")
lag = input.int(1)
line.new(bar_index, open, bar_index, close)
values = line.all[lag]
plot(na(values) ? -1 : array.size(values))
plot(na(values) ? 1 : 0)
plot(array.size(line.all))
"#;
    assert_eq!(
        Sha256::digest(source.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        "22cbc2ae912124a91be947a487924328f2952c678014510618067c03b667c541"
    );
    let program = compile_csil(source);
    let module = compile_chart_wasm_program(&program)
        .expect("the drawing-array missing-reference check must compile");
    assert_eq!(
        module.layout.execution_mode,
        pinewasm_aot::ExecutionMode::Native,
        "na(array) must execute natively; a fallback is not validation"
    );
    assert!(imports_of(&module.wasm).is_empty());
    assert_eq!(program.inputs.len(), 1);
    assert_eq!(program.inputs[0].id, 0);
    let data = MarketData {
        time: vec![
            1_700_000_000_000,
            1_700_000_060_000,
            1_700_000_120_000,
            1_700_000_180_000,
        ],
        open: vec![1.0, 2.0, 1.0, 2.0],
        high: vec![3.0; 4],
        low: vec![0.0; 4],
        close: vec![2.0, 1.0, 2.0, 1.0],
        volume: vec![1.0; 4],
        mintick: 0.01,
        ..MarketData::default()
    };
    for (lag, historical, missing) in [
        (0, [1.0, 2.0, 3.0, 4.0], [0.0; 4]),
        (1, [-1.0, 1.0, 2.0, 3.0], [1.0, 0.0, 0.0, 0.0]),
        (2, [-1.0, -1.0, 1.0, 2.0], [1.0, 1.0, 0.0, 0.0]),
    ] {
        let mut options = RunOptions::default();
        options.inputs.insert(0, InputValue::Int(lag));
        let reference = run(&program, &data, &options);
        assert_eq!(reference.bars_executed, 4);
        let chart = run_chart(&module, &program, &data, &options);
        let context = format!("drawing-array na check, lag {lag}");
        assert_run_matches(&reference, &chart, 4, &context);
        assert_eq!(chart.mismatch, Json::Null, "{context}: no fallback credit");
        assert_eq!(chart.plots.len(), 3, "{context}");
        for (slot, expected) in [historical, missing, [1.0, 2.0, 3.0, 4.0]]
            .iter()
            .enumerate()
        {
            assert_eq!(chart.plots[slot].len(), 4, "{context}");
            for (bar, expected) in expected.iter().enumerate() {
                let reference_value = &reference.plots[slot].values[bar];
                assert!(matches!(reference_value, Value::Float(actual) if actual.to_bits() == f64::to_bits(*expected)),
                    "{context}, interpreter plot {slot} bar {bar}: {reference_value:?} != {expected}");
                assert_eq!(
                    chart.plots[slot][bar].to_bits(),
                    f64::to_bits(*expected),
                    "{context}, native plot {slot} bar {bar}"
                );
            }
        }
    }
}

// Chart-context history regression cases. Each inline source is pinned by its hash.
// The sources compile at offsets 0 and 1 only; every expected vector is listed
// below for the default chart context (chart.is_standard is true). Offset 2 and
// the dynamic input-lag case are local regression checks.
// chart.bg_color and chart.fg_color history is rejected at compile time; see
// crates/pinewasm_compiler/tests/chart_context_history.rs.
mod chart_context_history_regressions {
    use super::*;
    use sha2::{Digest, Sha256};

    fn native(source: &str, source_sha256: &str) -> (CsilProgram, AotModule) {
        let actual_sha256 = Sha256::digest(source.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            actual_sha256, source_sha256,
            "chart-context source bytes changed"
        );
        let program = compile_csil(source);
        let module = compile_chart_wasm_program(&program)
            .expect("chart-type predicate history must compile to a linked native module");
        assert_eq!(
            module.layout.execution_mode,
            pinewasm_aot::ExecutionMode::Native,
            "chart-type history must execute the native bar loop"
        );
        assert!(
            imports_of(&module.wasm).is_empty(),
            "linked chart needs no external imports"
        );
        (program, module)
    }

    fn assert_finished(
        program: &CsilProgram,
        module: &AotModule,
        options: &RunOptions,
        expected: &[f64; 4],
        context: &str,
    ) {
        let data = MarketData {
            time: vec![
                1_700_000_000_000,
                1_700_000_060_000,
                1_700_000_120_000,
                1_700_000_180_000,
            ],
            open: vec![1.0, 2.0, 1.0, 2.0],
            high: vec![3.0; 4],
            low: vec![0.0; 4],
            close: vec![2.0, 1.0, 2.0, 1.0],
            volume: vec![1.0; 4],
            mintick: 0.01,
            ..MarketData::default()
        };
        let reference = run(program, &data, options);
        assert_eq!(
            reference.bars_executed, 4,
            "{context}: interpreter bar count"
        );
        let chart = run_chart(module, program, &data, options);
        // Includes every plot lane and all colors/styles/drawing/other wire
        // outputs. Status OK is required; handing back cannot pass.
        assert_run_matches(&reference, &chart, 4, context);
        assert_eq!(chart.mismatch, Json::Null, "{context}: no fallback reason");
        assert_eq!(chart.plots.len(), 1, "{context}: plot count");
        assert_eq!(chart.plots[0].len(), 4, "{context}: native lane length");
        for (bar, expected) in expected.iter().enumerate() {
            let reference_value = &reference.plots[0].values[bar];
            assert!(
                matches!(reference_value, Value::Float(actual) if actual.to_bits() == expected.to_bits()),
                "{context}: interpreter bar {bar}: {reference_value:?} != {expected}"
            );
            assert_eq!(
                chart.plots[0][bar].to_bits(),
                expected.to_bits(),
                "{context}: native bar {bar}"
            );
        }
    }

    #[test]
    fn all_seven_chart_type_predicates_keep_static_native_history_and_bool_warmup() {
        // Each source is pinned by hash at offsets 0 to 2.
        // Default chart.is_standard is true; all six other predicates are
        // false. Missing Boolean history produces false, including the first
        // one or two bars of chart.is_standard at offsets 1 and 2.
        let cases = [
            (
                "chart.is_standard",
                "Chart type predicate history family",
                [
                    "a5ffe1d0eca83109dd1abb100dc1e90b9435228e52080d3a3cd4949cdb855587",
                    "9149dcbed6c2adc0dede1dd96b7ecf61ff7315fb95c8d50405605b5aede3ca25",
                    "18137ac81951f455e8d26fd319403b88cd86e0cd0faefbe0e911e4cb41913674",
                ],
                [[1.0; 4], [0.0, 1.0, 1.0, 1.0], [0.0, 0.0, 1.0, 1.0]],
            ),
            (
                "chart.is_heikinashi",
                "Chart context history discriminator",
                [
                    "97d43827180c98585b985be660f2b855ab2738cd2bb6393204c8ed6b0431dc79",
                    "a995aa9e54b7c0e589c524c05d7e4953f3a0c0b00d9f1efcc23afde624492a11",
                    "a760f976c59f5adaa6da7f784a36edbb89abd9ea39caa5b061dac44c35137516",
                ],
                [[0.0; 4]; 3],
            ),
            (
                "chart.is_kagi",
                "Chart type predicate history family",
                [
                    "edf6661fc54d99b5e22d3c490479ab6119a7a6df79c73fc83fc68f24db105ab9",
                    "f8146a708fe5d8fba59697a968f2996406a606c9c4434b63e02b60efd5d6e0d7",
                    "ef49c56e0b8c5f19bffa9d1c1a31ddd0b1599f4b54ded0a11fc7cba53f626858",
                ],
                [[0.0; 4]; 3],
            ),
            (
                "chart.is_linebreak",
                "Chart type predicate history family",
                [
                    "2d1fa515e807b4961d5b0712b436720790dbd3d6b80171d0bfc2d760c97b0d9b",
                    "19e55db1141e8d1f9e08a96b1abe4f6e95df15c9021e87a0f2f2764f047da7cf",
                    "6f32664a42a8340cba6a2e2b72bc45b4eff36c9267405ac14dd10750805475bb",
                ],
                [[0.0; 4]; 3],
            ),
            (
                "chart.is_pnf",
                "Chart type predicate history family",
                [
                    "8e9d64b58c8638d2b0ca8b7214c30f552398f1aeb91857ac06e806d1f19c6b4a",
                    "04a753a3faaed20295fb650b8390bb6287f22b995d209b32116e7d06783755d5",
                    "33df028ba5a3ed7de7731e3e0e9308dfe502964469f528a86e4f5ee2614e7e17",
                ],
                [[0.0; 4]; 3],
            ),
            (
                "chart.is_range",
                "Chart type predicate history family",
                [
                    "0b5f35363f2a60a4e8324ba54f42a3638bc2d932119712a56524475888104d54",
                    "861507920c180ad31d5da470521b23ca7347fda98a38c3337a135f613ac29431",
                    "87cdec8d33b98e500b660e6df9c93433cd8deae719f9fb60776e25621320134d",
                ],
                [[0.0; 4]; 3],
            ),
            (
                "chart.is_renko",
                "Chart type predicate history family",
                [
                    "3f46d88741e657fc225d2247dab6e66bf0ee5404efccd5fffb551eaaaaf7a213",
                    "997a15039a0aa1db4b2b9939db3ee3978ef7ae9b1f58a9cb7bb1f650b89fa83b",
                    "6613519ff4287bb9c20848606b4497140fa5efa6929ec6b9feff6f290593275c",
                ],
                [[0.0; 4]; 3],
            ),
        ];
        for (predicate, title, hashes, expected) in cases {
            for offset in 0..=2 {
                let source = format!(
                    "//@version=6\nindicator(\"{title}\")\nplot({predicate}[{offset}] ? 1 : 0)\n"
                );
                let (program, module) = native(&source, hashes[offset]);
                assert!(
                    program.inputs.is_empty(),
                    "the static case has no input override"
                );
                assert_finished(
                    &program,
                    &module,
                    &RunOptions::default(),
                    &expected[offset],
                    &format!("{predicate}, static offset {offset}"),
                );
            }
        }
    }

    #[test]
    fn standard_chart_dynamic_native_history_respects_input_lag_overrides() {
        // One compiled source, three input selections. The existing context
        // remains standard; no chart-type override API is introduced here.
        let source = r#"//@version=6
indicator("Chart context history safeguards")
lag = input.int(1)
plot(chart.is_standard[lag] ? 1 : 0)
"#;
        let (program, module) = native(
            source,
            "5a48cace045d682cd811e7e36c60c6c0da35b1d3168c10b462b8601333eebfb4",
        );
        assert_eq!(program.inputs.len(), 1);
        assert_eq!(program.inputs[0].id, 0);
        for (lag, expected) in [
            (0, [1.0; 4]),
            (1, [0.0, 1.0, 1.0, 1.0]),
            (2, [0.0, 0.0, 1.0, 1.0]),
        ] {
            let mut options = RunOptions::default();
            options.inputs.insert(0, InputValue::Int(lag));
            assert_finished(
                &program,
                &module,
                &options,
                &expected,
                &format!("standard chart, dynamic offset {lag}"),
            );
        }
    }
}
