//! A second, unspliced implementation of `emit_prepare`, kept as the
//! byte-for-byte check for the `PrepareConsts` splice in `emit.rs`. The
//! `total_color_lanes` it uses is defined at the top of this file with the
//! same u32 sums.

use super::*;

fn total_color_lanes(layout: &Layout) -> u32 {
    layout.plot_color_roles.iter().sum::<u32>()
        + layout.visual_color_roles.iter().sum::<u32>()
        + layout.fill_color_roles.iter().sum::<u32>()
}

/// `prepare(bars) -> status`: guard bars, size memory, compute every lane
/// base into globals + header, default the budget/mintick cells.
#[allow(clippy::too_many_arguments)] // One extra scalar carries the per-module bar bound.
pub(super) fn emit_prepare_reference(
    program: &CsilProgram,
    layout: &Layout,
    slots: &SlotLayout,
    globals: &Globals,
    rt: Option<&runtime::Runtime>,
    lane_floor: u32,
    max_bars: u32,
    standalone_wasm: Option<&StandaloneWasmSpec>,
) -> Function {
    // locals: 0 = bars param; 1.. = region bases.
    let bars = 0u32;
    let builtin_base = 1u32;
    let slots_base = 2u32;
    let written_base = 3u32;
    let plots_base = 4u32;
    let colors_base = 5u32;
    let scratch_base = 6u32;
    let alerts_base = 7u32;
    let ta_base = 8u32;
    let heap_end = 9u32;
    let pages = 10u32;
    let mut f = Function::new([(10, ValType::I32)]);

    f.instruction(&I::LocalGet(bars))
        .instruction(&I::I32Const(max_bars as i32))
        .instruction(&I::I32GtU)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::I32Const(status::TOO_MANY_BARS))
        .instruction(&I::Return)
        .instruction(&I::End);

    f.instruction(&I::LocalGet(bars))
        .instruction(&I::GlobalSet(globals.bars));

    let n_inputs = program.inputs.len() as i32;
    let lane_count = BUILTIN_LANE_COUNT as i32
        + layout.source_inputs.len() as i32
        + layout.security_lanes as i32;
    let total_color_lanes: u32 = total_color_lanes(layout);

    // The linked numeric runtime owns a fixed block of low memory: its rodata
    // sits at addresses baked into its own code, and its shadow stack grows
    // down from the top of the block. `prepare` arms the stack pointer (the
    // linker's own initializer places the stack at address 0, on top of this
    // header) and starts the artifact's lanes above the block. The header and
    // input table fit below it — `emit` bounds the input count for that.
    if let Some(rt) = rt {
        f.instruction(&I::I32Const(rt.reserved_hi() as i32))
            .instruction(&I::GlobalSet(rt.stack_pointer_global()));
    }
    // builtin_base = header + input table (8 bytes per input), or the lane
    // floor when a linked runtime's reserved block (plus, in standalone WebAssembly mode, the
    // key table) sits above that.
    f.instruction(&I::I32Const(
        (header::SIZE as i32 + n_inputs * 8).max(lane_floor as i32),
    ))
    .instruction(&I::LocalSet(builtin_base));
    // slots_base = builtin_base + lane_count*8*bars
    f.instruction(&I::LocalGet(builtin_base))
        .instruction(&I::LocalGet(bars))
        .instruction(&I::I32Const(lane_count * 8))
        .instruction(&I::I32Mul)
        .instruction(&I::I32Add)
        .instruction(&I::LocalSet(slots_base));
    // written_base = slots_base + the complete slot-history region
    f.instruction(&I::LocalGet(slots_base))
        .instruction(&I::I32Const(slots.bytes as i32))
        .instruction(&I::I32Add)
        .instruction(&I::LocalSet(written_base));
    // plots_base = align8(written_base + slot_count)
    f.instruction(&I::LocalGet(written_base))
        .instruction(&I::I32Const(layout.slot_count as i32 + 7))
        .instruction(&I::I32Add)
        .instruction(&I::I32Const(!7))
        .instruction(&I::I32And)
        .instruction(&I::LocalSet(plots_base));
    // colors_base = plots_base + plot_count*8*bars
    f.instruction(&I::LocalGet(plots_base))
        .instruction(&I::LocalGet(bars))
        .instruction(&I::I32Const(layout.plot_count as i32 * 8))
        .instruction(&I::I32Mul)
        .instruction(&I::I32Add)
        .instruction(&I::LocalSet(colors_base));
    // scratch_base = align8(colors_base + total_color_lanes*4*bars)
    f.instruction(&I::LocalGet(colors_base))
        .instruction(&I::LocalGet(bars))
        .instruction(&I::I32Const(total_color_lanes as i32 * 4))
        .instruction(&I::I32Mul)
        .instruction(&I::I32Add)
        .instruction(&I::I32Const(7))
        .instruction(&I::I32Add)
        .instruction(&I::I32Const(!7))
        .instruction(&I::I32And)
        .instruction(&I::LocalSet(scratch_base));
    // alerts_base = scratch_base + the planned scratch size (the linked
    // engine's tuple extras and host.vm's cells share that region; ≥ 32
    // bytes).
    f.instruction(&I::LocalGet(scratch_base))
        .instruction(&I::I32Const((layout.scratch_cells * 8) as i32))
        .instruction(&I::I32Add)
        .instruction(&I::LocalSet(alerts_base));
    // ta_base = alerts_base + bars * sites * 8. Each alertcondition site
    // fires at most once per bar (analysis rejects one inside a loop), so
    // that product is an exact bound and `run` needs no capacity check.
    f.instruction(&I::LocalGet(alerts_base))
        .instruction(&I::LocalGet(bars))
        .instruction(&I::I32Const(
            (layout.alert_sites * header::ALERT_ENTRY) as i32,
        ))
        .instruction(&I::I32Mul)
        .instruction(&I::I32Add)
        .instruction(&I::LocalSet(ta_base));
    if let Some(g) = globals.ta_base {
        f.instruction(&I::LocalGet(ta_base))
            .instruction(&I::GlobalSet(g));
    }
    // heap_end = ta_base + the linked engine's region, a compile-time size
    // (every ring capacity is a baked const param, so nothing about it scales
    // with the bar count). With the native heap this doubles as the heap
    // region's base: the bump allocator starts here and `memory.grow` extends
    // at run time.
    f.instruction(&I::LocalGet(ta_base))
        .instruction(&I::I32Const(ta_region_bytes(layout) as i32))
        .instruction(&I::I32Add)
        .instruction(&I::LocalSet(heap_end));
    if let Some(g) = globals.heap {
        f.instruction(&I::LocalGet(heap_end))
            .instruction(&I::GlobalSet(g.base));
        f.instruction(&I::LocalGet(heap_end))
            .instruction(&I::GlobalSet(g.ptr));
    }

    // Grow memory if needed.
    f.instruction(&I::LocalGet(heap_end))
        .instruction(&I::I32Const(0xFFFF))
        .instruction(&I::I32Add)
        .instruction(&I::I32Const(16))
        .instruction(&I::I32ShrU)
        .instruction(&I::LocalSet(pages));
    f.instruction(&I::LocalGet(pages))
        .instruction(&I::MemorySize(0))
        .instruction(&I::I32GtU)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::LocalGet(pages))
        .instruction(&I::MemorySize(0))
        .instruction(&I::I32Sub)
        .instruction(&I::MemoryGrow(0))
        .instruction(&I::I32Const(-1))
        .instruction(&I::I32Eq)
        .instruction(&I::If(BlockType::Empty))
        .instruction(&I::I32Const(status::ALLOC_FAILED))
        .instruction(&I::Return)
        .instruction(&I::End)
        .instruction(&I::End);

    // Header writes.
    let store_u32 = |f: &mut Function, off: u32, push: &dyn Fn(&mut Function)| {
        f.instruction(&I::I32Const(0));
        push(f);
        f.instruction(&I::I32Store(mem_u32(off)));
    };
    store_u32(&mut f, header::OFF_MAGIC, &|f| {
        f.instruction(&I::I32Const(header::MAGIC as i32));
    });
    store_u32(&mut f, header::OFF_ABI_VERSION, &|f| {
        f.instruction(&I::I32Const(header::ABI_VERSION as i32));
    });
    store_u32(&mut f, header::OFF_BARS, &|f| {
        f.instruction(&I::LocalGet(bars));
    });
    store_u32(&mut f, header::OFF_RING_MASK, &|f| {
        f.instruction(&I::I32Const(layout.mask() as i32));
    });
    store_u32(&mut f, header::OFF_BUILTIN_BASE, &|f| {
        f.instruction(&I::LocalGet(builtin_base));
    });
    store_u32(&mut f, header::OFF_SLOTS_BASE, &|f| {
        f.instruction(&I::LocalGet(slots_base));
    });
    store_u32(&mut f, header::OFF_PLOTS_BASE, &|f| {
        f.instruction(&I::LocalGet(plots_base));
    });
    store_u32(&mut f, header::OFF_COLORS_BASE, &|f| {
        f.instruction(&I::LocalGet(colors_base));
    });
    store_u32(&mut f, header::OFF_HEAP_END, &|f| {
        f.instruction(&I::LocalGet(heap_end));
    });
    store_u32(&mut f, header::OFF_ERR_CODE, &|f| {
        f.instruction(&I::I32Const(status::OK));
    });
    store_u32(&mut f, header::OFF_SCRATCH_BASE, &|f| {
        f.instruction(&I::LocalGet(scratch_base));
    });
    store_u32(&mut f, header::OFF_ALERTS_BASE, &|f| {
        f.instruction(&I::LocalGet(alerts_base));
    });
    store_u32(&mut f, header::OFF_ALERT_COUNT, &|f| {
        f.instruction(&I::I32Const(0));
    });
    f.instruction(&I::I32Const(0))
        .instruction(&I::I64Const(DEFAULT_BUDGET))
        .instruction(&I::I64Store(mem_u64(header::OFF_BUDGET_PER_BAR)));
    f.instruction(&I::I32Const(0))
        .instruction(&I::F64Const(DEFAULT_MINTICK.into()))
        .instruction(&I::F64Store(mem_f64_at(header::OFF_MINTICK)));
    if globals.heap.is_some() {
        let max_collection_elements = standalone_wasm
            .map_or(crate::heap_defaults::MAX_ELEMENTS, |spec| {
                spec.max_collection_elements
            });
        f.instruction(&I::I32Const(0))
            .instruction(&I::I64Const(max_collection_elements as i64))
            .instruction(&I::I64Store(mem_u64(header::OFF_MAX_ELEMS)));
        f.instruction(&I::I32Const(0))
            .instruction(&I::I64Const(crate::heap_defaults::MAX_OBJECTS as i64))
            .instruction(&I::I64Store(mem_u64(header::OFF_MAX_OBJS)));
    }

    // Lane-base globals.
    for (lane, cell) in globals.builtin.iter().enumerate() {
        if let Some(g) = cell {
            f.instruction(&I::LocalGet(builtin_base))
                .instruction(&I::LocalGet(bars))
                .instruction(&I::I32Const((lane * 8) as i32))
                .instruction(&I::I32Mul)
                .instruction(&I::I32Add)
                .instruction(&I::GlobalSet(*g));
        }
    }
    for (j, &input) in layout.source_inputs.iter().enumerate() {
        let g = globals.source_lane_global(input).expect("planned");
        f.instruction(&I::LocalGet(builtin_base))
            .instruction(&I::LocalGet(bars))
            .instruction(&I::I32Const(((BUILTIN_LANE_COUNT as usize + j) * 8) as i32))
            .instruction(&I::I32Mul)
            .instruction(&I::I32Add)
            .instruction(&I::GlobalSet(g));
    }
    for (k, &g) in globals.security_lanes().iter().enumerate() {
        let lane = BUILTIN_LANE_COUNT as usize + layout.source_inputs.len() + k;
        f.instruction(&I::LocalGet(builtin_base))
            .instruction(&I::LocalGet(bars))
            .instruction(&I::I32Const((lane * 8) as i32))
            .instruction(&I::I32Mul)
            .instruction(&I::I32Add)
            .instruction(&I::GlobalSet(g));
    }
    for (slot, &g) in globals.slot.iter().enumerate() {
        f.instruction(&I::LocalGet(slots_base))
            .instruction(&I::I32Const(slots.offsets[slot] as i32))
            .instruction(&I::I32Add)
            .instruction(&I::GlobalSet(g));
    }
    for (plot, &g) in globals.plot.iter().enumerate() {
        f.instruction(&I::LocalGet(plots_base))
            .instruction(&I::LocalGet(bars))
            .instruction(&I::I32Const((plot * 8) as i32))
            .instruction(&I::I32Mul)
            .instruction(&I::I32Add)
            .instruction(&I::GlobalSet(g));
    }
    let mut color_lane = 0i32;
    let mut set_color_lane = |f: &mut Function, g: u32| {
        f.instruction(&I::LocalGet(colors_base))
            .instruction(&I::LocalGet(bars))
            .instruction(&I::I32Const(color_lane * 4))
            .instruction(&I::I32Mul)
            .instruction(&I::I32Add)
            .instruction(&I::GlobalSet(g));
        color_lane += 1;
    };
    for g in globals.color_lanes() {
        set_color_lane(&mut f, g);
    }
    f.instruction(&I::LocalGet(written_base))
        .instruction(&I::GlobalSet(globals.written_base));
    f.instruction(&I::LocalGet(scratch_base))
        .instruction(&I::GlobalSet(globals.scratch_base));
    // Scratch cell 0 is idle between `prepare` and `run`: seed the cumulative
    // run budget there without changing ABI v6's header layout. A loader may
    // overwrite it after `prepare`; `run` copies it into a local before any
    // `host.vm` call reuses the cell for per-bar budget exchange. A
    // standalone WebAssembly module seeds the larger of the default and its
    // per-bar allowance times the bars evaluated, as `emit_prepare` does.
    f.instruction(&I::LocalGet(scratch_base));
    if standalone_wasm.is_some() {
        for _ in 0..2 {
            f.instruction(&I::LocalGet(bars))
                .instruction(&I::I64ExtendI32U)
                .instruction(&I::I64Const(STANDALONE_WASM_RUN_BUDGET_PER_BAR))
                .instruction(&I::I64Mul)
                .instruction(&I::I64Const(DEFAULT_RUN_BUDGET));
        }
        f.instruction(&I::I64GtU).instruction(&I::Select);
    } else {
        f.instruction(&I::I64Const(DEFAULT_RUN_BUDGET));
    }
    f.instruction(&I::I64Store(mem_u64(header::SCRATCH_OFF_BUDGET_PER_RUN)));

    // Zero the written flags (fresh run semantics even on instance reuse).
    f.instruction(&I::LocalGet(written_base))
        .instruction(&I::I32Const(0))
        .instruction(&I::I32Const(layout.slot_count as i32))
        .instruction(&I::MemoryFill(0));

    f.instruction(&I::I32Const(status::OK))
        .instruction(&I::Return)
        .instruction(&I::End);
    f
}

thread_local! {
    /// `prepare` functions compared on this test thread.
    static COMPARED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many `prepare` functions [`assert_matches`] has compared on this
/// thread.
pub(super) fn compared() -> usize {
    COMPARED.with(std::cell::Cell::get)
}

/// Panic unless `prepare` encodes to exactly the bytes the baseline
/// emitter produces for the same plan. Called by every test-build `emit`.
#[allow(clippy::too_many_arguments)] // Mirrors the baseline emitter's inputs.
pub(super) fn assert_matches(
    prepare: &Function,
    program: &CsilProgram,
    layout: &Layout,
    slots: &SlotLayout,
    globals: &Globals,
    rt: Option<&runtime::Runtime>,
    lane_floor: u32,
    max_bars: u32,
    standalone_wasm: Option<&StandaloneWasmSpec>,
) {
    use wasm_encoder::Encode;
    let reference = emit_prepare_reference(
        program,
        layout,
        slots,
        globals,
        rt,
        lane_floor,
        max_bars,
        standalone_wasm,
    );
    let mut expected = Vec::new();
    reference.encode(&mut expected);
    let mut actual = Vec::new();
    prepare.encode(&mut actual);
    assert!(
        actual == expected,
        "PrepareConsts splice changed the emitted prepare function"
    );
    COMPARED.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
mod tests {
    use super::compared;
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

    /// Every Pine fixture program in the test directories, through every emit mode that reaches
    /// `emit_prepare`: each emitted `prepare` must be byte-identical to the
    /// baseline emitter's (`assert_matches` panics otherwise).
    #[test]
    fn repository_programs_emit_the_reference_prepare() {
        let mut files = Vec::new();
        for dir in [
            "../../tests/compile-pass",
            "../../tests/regression",
            "../../benchmarks/workloads",
        ] {
            collect_pine_files(Path::new(dir), &mut files);
        }
        files.sort();
        assert!(files.len() >= 60, "fixture corpus went missing");

        let before = compared();
        for path in &files {
            let source = std::fs::read_to_string(path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
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
        let emitted = compared() - before;
        eprintln!(
            "[prepare-copy] files={} prepare-compared={emitted}",
            files.len()
        );
        assert!(emitted >= 60, "only {emitted} prepare functions compared");
    }
}
