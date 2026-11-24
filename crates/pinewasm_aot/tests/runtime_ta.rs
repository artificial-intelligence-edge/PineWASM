//! The linked ta engine must be bit-identical to [`pinewasm_ta`].
//!
//! This is the technical-analysis counterpart to `runtime_numerics.rs`, and
//! is load-bearing for the same reason: emitted artifacts no longer call back
//! into the host for `ta.ema` and friends; they call a WebAssembly build of
//! the crate the interpreter uses.
//! That is only sound if the two builds agree in every bit, so this drives
//! **every** engine function through both and compares `to_bits()`.
//!
//! Two distinct things are checked, and the second is why a sweep beats the
//! end-to-end differentials in `differential.rs`:
//!
//! 1. The two builds of the same Rust source agree. They do, because Rust
//!    never contracts or reassociates float arithmetic — but that is a
//!    property worth failing loudly on rather than assuming.
//! 2. The checked-in `runtime/pinewasm_aot_runtime_ta.wasm` is a build of
//!    the *current* `pinewasm_ta`. A stale blob fails here. The
//!    differentials would only catch staleness in a function one of them
//!    happens to exercise; this covers the whole table, so forgetting
//!    `cargo xtask aot-runtime-all` cannot ship quietly.

use pinewasm_aot::runtime;
use pinewasm_ta::{Decl, Engine, Lanes, Layout, State, TaFunc};

/// A synthetic OHLCV feed with enough shape to move every branch: a trend, a
/// reversal, flat bars (which divide by zero in several formulas), a zero
/// volume, and na holes in each lane.
struct Feed {
    open: Vec<f64>,
    high: Vec<f64>,
    low: Vec<f64>,
    close: Vec<f64>,
    volume: Vec<f64>,
    /// Set by [`Feed::signed_zeros`]: the operands are then the raw lane
    /// values, so a `-0.0` reaches the engine as `-0.0`.
    signed_zeros: bool,
}

impl Feed {
    fn build(bars: usize) -> Feed {
        let mut feed = Feed {
            open: Vec::with_capacity(bars),
            high: Vec::with_capacity(bars),
            low: Vec::with_capacity(bars),
            close: Vec::with_capacity(bars),
            volume: Vec::with_capacity(bars),
            signed_zeros: false,
        };
        let mut state = 0x5EED_1234_ABCD_0001u64;
        let mut price = 100.0f64;
        for i in 0..bars {
            // SplitMix64's finalizer: reproducible across platforms, which
            // matters for a test whose failures are last-ulp differences.
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            let step = (z % 2001) as f64 / 1000.0 - 1.0;
            price += step;
            let flat = i % 23 == 7;
            let (o, c) = (price, if flat { price } else { price + step / 2.0 });
            let (h, l) = if flat {
                (price, price)
            } else {
                (o.max(c) + 0.25, o.min(c) - 0.25)
            };
            // na holes, one lane at a time, plus a zero-volume bar.
            let hole = i % 31 == 13;
            feed.open
                .push(if hole && i % 5 == 3 { f64::NAN } else { o });
            feed.high
                .push(if hole && i % 5 == 0 { f64::NAN } else { h });
            feed.low.push(if hole && i % 5 == 1 { f64::NAN } else { l });
            feed.close
                .push(if hole && i % 5 == 2 { f64::NAN } else { c });
            feed.volume.push(match i % 37 {
                5 => 0.0,
                11 => f64::NAN,
                _ => 1000.0 + (z % 5000) as f64,
            });
        }
        feed
    }

    /// A feed whose every lane is drawn from `{+0.0, -0.0, 1.0, -1.0, na}`,
    /// zeros of both signs being the common case. A window, a running
    /// extremum or a high/low pair then regularly holds `+0.0` and `-0.0`
    /// together, which is the tie `f64::max`/`f64::min` leave unspecified
    /// and the engine decides.
    fn signed_zeros(bars: usize, seed: u64) -> Feed {
        const ALPHABET: [f64; 8] = [0.0, -0.0, 0.0, -0.0, -0.0, 1.0, -1.0, f64::NAN];
        let mut state = seed;
        let mut next = move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        let mut lane = |zero_run: bool| -> Vec<f64> {
            (0..bars)
                .map(|i| {
                    let z = next();
                    // 64-bar stretches, in turn: mixed signed zeros only
                    // (no na, no ±1, so windows of every tested length fill
                    // with both zeros), all `-0.0`, the full alphabet, all
                    // `+0.0`. The constant stretches give long windows of
                    // nothing but `-0.0`, whose extremum is still `+0.0`.
                    match (zero_run, (i / 64) % 4) {
                        (true, 0) => ALPHABET[(z % 5) as usize],
                        (true, 1) => -0.0,
                        (true, 3) => 0.0,
                        _ => ALPHABET[(z % 8) as usize],
                    }
                })
                .collect()
        };
        Feed {
            open: lane(true),
            high: lane(true),
            low: lane(true),
            close: lane(true),
            volume: lane(false),
            signed_zeros: true,
        }
    }

    fn bars(&self) -> u32 {
        self.close.len() as u32
    }
}

impl Lanes for Feed {
    fn open(&self, bar: u32) -> f64 {
        self.open.get(bar as usize).copied().unwrap_or(f64::NAN)
    }
    fn high(&self, bar: u32) -> f64 {
        self.high.get(bar as usize).copied().unwrap_or(f64::NAN)
    }
    fn low(&self, bar: u32) -> f64 {
        self.low.get(bar as usize).copied().unwrap_or(f64::NAN)
    }
    fn close(&self, bar: u32) -> f64 {
        self.close.get(bar as usize).copied().unwrap_or(f64::NAN)
    }
    fn volume(&self, bar: u32) -> f64 {
        self.volume.get(bar as usize).copied().unwrap_or(f64::NAN)
    }
    fn time_ms(&self, bar: u32) -> i64 {
        1_577_836_800_000 + i64::from(bar) * 86_400_000
    }
}

/// The `ta` blob, instantiated and armed the way `prepare` arms it.
struct Blob {
    store: wasmi::Store<()>,
    memory: wasmi::Memory,
    init: wasmi::TypedFunc<(u32, u32, u32, u32, u32, u32), i32>,
    advance: wasmi::TypedFunc<(u32, u32, f64, f64, f64), f64>,
    /// First free byte above the blob's rodata and shadow stack.
    free: u32,
}

/// Shadow stack the artifact's `prepare` hands the spliced runtime.
const STACK_SIZE: u32 = 16 * 1024;

impl Blob {
    fn load() -> Blob {
        let engine = wasmi::Engine::default();
        let module = wasmi::Module::new(&engine, runtime::MODULE_WASM_TA)
            .expect("the ta runtime module is valid wasm");
        let mut store = wasmi::Store::new(&engine, ());
        // No linker entries at all: the blob imports nothing, which is the
        // whole point of it.
        let instance = <wasmi::Linker<()>>::new(&engine)
            .instantiate_and_start(&mut store, &module)
            .expect("ta runtime module instantiates");
        let memory = instance
            .get_memory(&store, "memory")
            .expect("the blob exports its memory");

        // Arm the shadow stack above the rodata, exactly as `emit_prepare`
        // does, then start our own data above it. Without this the engine's
        // stack frames would grow down through address 0.
        let heap_base = match instance
            .get_global(&store, "__heap_base")
            .expect("the blob exports __heap_base")
            .get(&store)
        {
            wasmi::Val::I32(v) => v as u32,
            other => panic!("__heap_base is {other:?}"),
        };
        let reserved_hi = ((heap_base + 15) & !15) + STACK_SIZE;
        instance
            .get_global(&store, "__stack_pointer")
            .expect("the blob exports __stack_pointer")
            .set(&mut store, wasmi::Val::I32(reserved_hi as i32))
            .expect("the stack pointer is mutable");

        let init = instance
            .get_typed_func(&store, "pinewasm_rt_ta_init")
            .expect("the blob exports pinewasm_rt_ta_init");
        let advance = instance
            .get_typed_func(&store, "pinewasm_rt_ta_advance")
            .expect("the blob exports pinewasm_rt_ta_advance");
        Blob {
            store,
            memory,
            init,
            advance,
            free: (reserved_hi + 7) & !7,
        }
    }

    fn grow_to(&mut self, bytes: u32) {
        let pages = u64::from(bytes.div_ceil(65_536));
        let have = self.memory.size(&self.store);
        if pages > have {
            self.memory
                .grow(&mut self.store, pages - have)
                .expect("memory grows");
        }
    }

    fn write(&mut self, addr: u32, bytes: &[u8]) {
        self.memory
            .write(&mut self.store, addr as usize, bytes)
            .expect("write inside memory");
    }
}

/// Run `decls` over `feed` on the host, returning every result in bar order
/// (value then the two tuple extras, so a divergence in any element shows).
fn host_run(decls: &[Decl], feed: &Feed) -> Vec<[f64; 3]> {
    let layout = Layout::plan(decls);
    let mut states = vec![State::Invalid; decls.len()];
    let mut cells = vec![f64::NAN; layout.cells as usize];
    let mut scratch = vec![f64::NAN; layout.scratch as usize];
    let mut engine = Engine::init(decls, &mut states, &mut cells, &mut scratch);
    let mut out = Vec::new();
    for bar in 0..feed.bars() {
        for (index, _) in decls.iter().enumerate() {
            let args = operands(feed, bar, index as u32);
            let result = engine
                .advance(index as u32, args, feed, bar)
                .expect("advanceable");
            out.push([result.value, result.extra[0], result.extra[1]]);
        }
    }
    out
}

/// The same run inside the blob, driving its exports over its own memory.
fn blob_run(decls: &[Decl], feed: &Feed) -> Vec<[f64; 3]> {
    let mut blob = Blob::load();
    let layout = Layout::plan(decls);

    // Lay out: decl table, then six lanes, then the extras cell, then the
    // engine region — the same shapes `prepare` reserves, at addresses this
    // test picks.
    let decl_table = blob.free;
    let lanes = decl_table + (decls.len() as u32 * Decl::SIZE as u32).next_multiple_of(8);
    let bars = feed.bars();
    let extras = lanes + 6 * bars * 8;
    let region = extras + 32;
    blob.grow_to(region + layout.region_bytes() + 64);

    let mut table = Vec::new();
    for decl in decls {
        table.extend_from_slice(&decl.to_le_bytes());
    }
    blob.write(decl_table, &table);
    for (index, lane) in [&feed.open, &feed.high, &feed.low, &feed.close, &feed.volume]
        .iter()
        .enumerate()
    {
        let base = lanes + index as u32 * bars * 8;
        for (bar, value) in lane.iter().enumerate() {
            blob.write(base + bar as u32 * 8, &value.to_le_bytes());
        }
    }
    // The time lane, as f64, exactly as the loader writes it.
    let time_base = lanes + 5 * bars * 8;
    for bar in 0..bars {
        let t = feed.time_ms(bar) as f64;
        blob.write(time_base + bar * 8, &t.to_le_bytes());
    }

    let status = blob
        .init
        .call(
            &mut blob.store,
            (region, decl_table, decls.len() as u32, lanes, bars, extras),
        )
        .expect("pinewasm_rt_ta_init does not trap");
    assert_eq!(
        status, 0,
        "pinewasm_rt_ta_init refused the declaration table"
    );

    let mut out = Vec::new();
    let mut buf = [0u8; 16];
    for bar in 0..bars {
        for index in 0..decls.len() as u32 {
            let [a, b, c] = operands(feed, bar, index);
            let value = blob
                .advance
                .call(&mut blob.store, (index, bar, a, b, c))
                .expect("pinewasm_rt_ta_advance does not trap");
            blob.memory
                .read(&blob.store, extras as usize, &mut buf)
                .expect("read the extras cells");
            out.push([
                value,
                f64::from_le_bytes(buf[0..8].try_into().unwrap()),
                f64::from_le_bytes(buf[8..16].try_into().unwrap()),
            ]);
        }
    }
    out
}

/// Operands for site `index` on `bar`. Derived from the feed so both sides
/// see identical inputs, and varied per site so two sites of the same
/// function do not receive the same series.
fn operands(feed: &Feed, bar: u32, index: u32) -> [f64; 3] {
    if feed.signed_zeros {
        // Raw lane values: `close + skew` would turn `-0.0` into `+0.0`.
        // The lanes are rotated per site, so sites still differ.
        let lanes = [&feed.close, &feed.open, &feed.high, &feed.low];
        let pick = |k: u32| {
            lanes[((index + k) % 4) as usize]
                .get(bar as usize)
                .copied()
                .unwrap_or(f64::NAN)
        };
        return [pick(0), pick(1), pick(2)];
    }
    let close = feed.close(bar);
    let open = feed.open(bar);
    let skew = f64::from(index % 3) * 0.125;
    [
        close + skew,
        open - skew,
        // A third operand that is a plain 0/1 as often as it is numeric:
        // `ta.stoch` wants a number here, `ta.vwap`'s banded form a mult, and
        // the condition-taking functions a bool.
        if bar % 5 == 0 { 1.0 } else { 0.0 },
    ]
}

/// Const params for one function, chosen to be valid under its
/// `engine_param_rules` and to exercise a real window rather than a
/// degenerate one.
fn params_for(func: TaFunc) -> Vec<i64> {
    params_with_lengths(func, 9, 4)
}

/// [`params_for`] with the window lengths chosen by the caller: `first` for
/// the first const param, `rest` for any later length.
fn params_with_lengths(func: TaFunc, first: i64, rest: i64) -> Vec<i64> {
    func.engine_param_rules()
        .iter()
        .enumerate()
        .map(|(i, rule)| match rule {
            pinewasm_ta::TaParamRule::Length { min } => {
                (*min).max(if i == 0 { first } else { rest })
            }
            pinewasm_ta::TaParamRule::IntRange { min, max } => (*min).max(1).min(*max),
            pinewasm_ta::TaParamRule::FloatBits { positive } => {
                if *positive {
                    6.0f64.to_bits() as i64
                } else {
                    2.0f64.to_bits() as i64
                }
            }
            pinewasm_ta::TaParamRule::Percent => 85.0f64.to_bits() as i64,
            pinewasm_ta::TaParamRule::Bool01 => i64::from(i % 2 == 0),
        })
        .collect()
}

#[test]
fn every_engine_function_agrees_between_the_wasm_and_host_builds() {
    let feed = Feed::build(400);
    // Every engine-backed function. `ta.change`/`ta.sma` are slot-based and
    // never appear in the table, so they are the only exclusions — and
    // `is_engine` is the engine's own answer to that question, not a list
    // this test maintains.
    let decls: Vec<Decl> = TaFunc::ALL
        .iter()
        .filter(|func| func.is_engine())
        .map(|func| Decl::new(*func, &params_for(*func)).expect("declaration fits"))
        .collect();
    assert!(
        decls.len() > 60,
        "expected the whole engine table, got {} functions",
        decls.len()
    );

    let host = host_run(&decls, &feed);
    let blob = blob_run(&decls, &feed);
    assert_eq!(host.len(), blob.len());

    let mut diverged = Vec::new();
    for (i, (h, b)) in host.iter().zip(&blob).enumerate() {
        let bar = i / decls.len();
        let site = i % decls.len();
        for element in 0..3 {
            if h[element].to_bits() != b[element].to_bits() {
                let func = decls[site].func().expect("known function");
                diverged.push(format!(
                    "{func:?} bar {bar} element {element}: host {:?} ({:#018x}) vs wasm {:?} ({:#018x})",
                    h[element],
                    h[element].to_bits(),
                    b[element],
                    b[element].to_bits()
                ));
            }
        }
    }
    assert!(
        diverged.is_empty(),
        "the wasm and host builds of the ta engine disagree in {} place(s).\n\
         If `pinewasm_ta` changed, the checked-in blob is stale: run \
         `cargo xtask aot-runtime-all`.\n{}",
        diverged.len(),
        diverged
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Signed-zero agreement: `f64::max`/`f64::min` leave the `+0.0` vs `-0.0` tie
/// to the target, and the arm64 host and the wasm blob used to pick
/// different zeros (`ta.highest(x, 2)` over `[-0.0, +0.0]` was `+0.0` on the
/// host and `-0.0` in the blob). The engine now decides: window extrema
/// never return `-0.0`, and the once-per-bar maxima and minima follow IEEE
/// 754-2019 `maximum`/`minimum`. So every engine function must agree bit
/// for bit on feeds made of signed zeros, at window lengths on both sides
/// of the long-window switch at 32.
#[test]
fn every_engine_function_agrees_on_signed_zero_feeds() {
    let mut diverged = Vec::new();
    let mut functions = std::collections::BTreeSet::new();
    // What the feeds reached, so the test cannot pass by never producing a
    // zero: zero results from `ta.highest` and `ta.lowest` on each side of
    // the switch, counted by sign. All of them must be `+0.0`.
    let mut zero_results = [[0usize; 2]; 4];
    for (round, (first, rest)) in [(2i64, 2i64), (3, 2), (9, 4), (31, 5), (32, 7), (40, 33)]
        .into_iter()
        .enumerate()
    {
        let feed = Feed::signed_zeros(320, 0xA5A5_0000_0000_0001 + round as u64);
        let decls: Vec<Decl> = TaFunc::ALL
            .iter()
            .filter(|func| func.is_engine())
            .map(|func| {
                Decl::new(*func, &params_with_lengths(*func, first, rest))
                    .expect("declaration fits")
            })
            .collect();
        assert!(decls.len() > 60, "got {} functions", decls.len());
        let host = host_run(&decls, &feed);
        let blob = blob_run(&decls, &feed);
        assert_eq!(host.len(), blob.len());
        for (i, (h, b)) in host.iter().zip(&blob).enumerate() {
            let bar = i / decls.len();
            let func = decls[i % decls.len()].func().expect("known function");
            let slot = match func {
                TaFunc::Highest => Some(0),
                TaFunc::Lowest => Some(1),
                _ => None,
            };
            if let Some(slot) = slot {
                if h[0] == 0.0 {
                    let side = usize::from(first >= 32);
                    zero_results[2 * side + slot][usize::from(h[0].is_sign_negative())] += 1;
                }
            }
            for element in 0..3 {
                if h[element].to_bits() != b[element].to_bits() {
                    functions.insert(format!("{func:?}"));
                    diverged.push(format!(
                        "{func:?} lengths ({first}, {rest}) bar {bar} element {element}: \
                         host {:?} ({:#018x}) vs wasm {:?} ({:#018x})",
                        h[element],
                        h[element].to_bits(),
                        b[element],
                        b[element].to_bits()
                    ));
                }
            }
        }
    }
    assert!(
        diverged.is_empty(),
        "the wasm and host builds of the ta engine disagree on signed-zero feeds in {} \
         place(s), in {functions:?}.\n{}",
        diverged.len(),
        diverged
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        zero_results
            .iter()
            .all(|signs| signs[0] > 0 && signs[1] == 0),
        "the feeds must reach zero extrema on both sides of the switch, none of them -0.0: \
         {zero_results:?}"
    );
}

#[test]
fn the_blob_surface_is_exactly_what_the_emitter_splices() {
    // The emitter resolves `pinewasm_rt_ta_init`/`pinewasm_rt_ta_advance` by name and refuses a
    // blob that declares a table, an element section, an import, or a start
    // function (the splice does not account for any of them). Loading it here
    // proves the checked-in bytes still satisfy that, independently of a
    // compile actually happening.
    let blob = Blob::load();
    assert!(blob.free > 0, "the shadow stack was armed above the rodata");
    assert!(
        wasmi::Module::new(&wasmi::Engine::default(), runtime::MODULE_WASM_TA).is_ok(),
        "the ta blob is valid wasm"
    );
}
