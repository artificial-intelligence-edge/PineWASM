//! Blob-staleness differentials for the runtime's standalone WebAssembly family.
//!
//! `tests/runtime_numerics.rs` pins the checked-in blob's *numeric* exports
//! against `pinewasm_num`; nothing there touches the `standalone_wasm_*` family, so a
//! stale blob whose JSON scanner or f64 printer predates a fix would sail
//! through that file. This one instantiates the standalone WebAssembly blob (`MODULE_WASM_STANDALONE_WASM`
//! — the only one of the two checked-in blobs carrying the `standalone_wasm_*` family)
//! under wasmi and compares:
//!
//! 1. the blob's serializer output against the host build of the *same*
//!    `ryu`/`itoa` versions (pinned `=1.0.23`/`=1.0.18` in both places), with
//!    parse-back bit equality on top, and
//! 2. the blob's JSON scanner against `serde_json` over an
//!    acceptance/rejection corpus, with the three deliberate divergences
//!    (depth bound, numeric-range leniency, lone surrogate escapes)
//!    asserted rather than skipped.
//!
//! Plus the two facts the splice depends on but cannot check from the
//! workspace side without help: rodata stays at/above 4096 (the
//! `Runtime::parse` bound — a violation there is a `OnceLock` panic in
//! production code paths, not a clean `Err`), and the blob's manifests
//! keep the exact pins that make it reproducible.

use pinewasm_aot::runtime;

/// The blob's own CSAW-header mirror (`runtime/src/standalone_wasm.rs`): serialize reads
/// bar count and region bases from these fixed offsets.
const OFF_BARS: u32 = 0x08;
const OFF_BUILTIN_BASE: u32 = 0x10;
const OFF_PLOTS_BASE: u32 = 0x18;

struct Blob {
    store: wasmi::Store<()>,
    instance: wasmi::Instance,
    memory: wasmi::Memory,
}

impl Blob {
    fn load() -> Blob {
        let engine = wasmi::Engine::default();
        let module = wasmi::Module::new(&engine, runtime::MODULE_WASM_STANDALONE_WASM)
            .expect("the standalone WebAssembly runtime module is valid wasm");
        let mut store = wasmi::Store::new(&engine, ());
        let instance = <wasmi::Linker<()>>::new(&engine)
            .instantiate_and_start(&mut store, &module)
            .expect("runtime module instantiates");
        let memory = instance
            .get_memory(&store, "memory")
            .expect("the blob exports memory");
        Blob {
            store,
            instance,
            memory,
        }
    }

    fn reset(&mut self) {
        self.instance
            .get_typed_func::<(), ()>(&self.store, "standalone_wasm_reset")
            .expect("standalone_wasm_reset export")
            .call(&mut self.store, ())
            .expect("standalone_wasm_reset call");
    }

    fn alloc(&mut self, len: usize) -> u32 {
        let ptr = self.alloc_raw(len as u32);
        assert_ne!(
            ptr, 0,
            "standalone_wasm_alloc must not fail for {len} bytes"
        );
        ptr
    }

    fn alloc_raw(&mut self, len: u32) -> u32 {
        self.instance
            .get_typed_func::<i32, i32>(&self.store, "standalone_wasm_alloc")
            .expect("standalone_wasm_alloc export")
            .call(&mut self.store, len as i32)
            .expect("standalone_wasm_alloc call") as u32
    }

    fn dealloc_raw(&mut self, ptr: u32, len: u32) {
        self.instance
            .get_typed_func::<(i32, i32), ()>(&self.store, "standalone_wasm_dealloc")
            .expect("standalone_wasm_dealloc export")
            .call(&mut self.store, (ptr as i32, len as i32))
            .expect("standalone_wasm_dealloc call");
    }

    fn relocate_raw(&mut self, minimum_base: u32) -> i32 {
        self.instance
            .get_typed_func::<i32, i32>(&self.store, "standalone_wasm_arena_reloc")
            .expect("standalone_wasm_arena_reloc export")
            .call(&mut self.store, minimum_base as i32)
            .expect("standalone_wasm_arena_reloc call")
    }

    fn write(&mut self, at: u32, bytes: &[u8]) {
        self.memory.data_mut(&mut self.store)[at as usize..at as usize + bytes.len()]
            .copy_from_slice(bytes);
    }

    fn last_error(&mut self) -> Option<String> {
        let packed = self
            .instance
            .get_typed_func::<(), i64>(&self.store, "standalone_wasm_last_error")
            .expect("standalone_wasm_last_error export")
            .call(&mut self.store, ())
            .expect("standalone_wasm_last_error call") as u64;
        if packed == 0 {
            return None;
        }
        let (ptr, len) = ((packed >> 32) as usize, (packed & 0xFFFF_FFFF) as usize);
        let mem = self.memory.data(&self.store);
        Some(String::from_utf8(mem[ptr..ptr + len].to_vec()).expect("error is UTF-8"))
    }

    /// Run the whole serializer over a one-bar window whose close is `price`
    /// and whose plot posture is +1, returning the complete OUT document.
    fn serialize_one(&mut self, price: f64, time: i64) -> String {
        self.reset();
        // Lanes live in an arena allocation; the output lands above it, so
        // nothing here needs to know the blob's memory map.
        let lanes = self.alloc(56);
        self.write(OFF_BARS, &1u32.to_le_bytes());
        self.write(OFF_BUILTIN_BASE, &lanes.to_le_bytes());
        self.write(OFF_PLOTS_BASE, &(lanes + 48).to_le_bytes());
        self.write(lanes + 3 * 8, &price.to_le_bytes()); // close lane
        self.write(lanes + 5 * 8, &(time as f64).to_le_bytes()); // time lane
        self.write(lanes + 48, &1.0f64.to_le_bytes()); // plot 0, bar 0
        let packed = self
            .instance
            .get_typed_func::<(i32, i32, i32), i64>(&self.store, "standalone_wasm_serialize")
            .expect("standalone_wasm_serialize export")
            .call(&mut self.store, (0, 1, 0))
            .expect("standalone_wasm_serialize call") as u64;
        assert_ne!(packed, 0, "serialize must succeed for finite {price:?}");
        let (ptr, len) = ((packed >> 32) as usize, (packed & 0xFFFF_FFFF) as usize);
        let mem = self.memory.data(&self.store);
        String::from_utf8(mem[ptr..ptr + len].to_vec()).expect("OUT is UTF-8")
    }

    /// Feed one document to `standalone_wasm_scan`.
    fn scan(&mut self, doc: &str) -> Result<i64, String> {
        self.reset();
        let ptr = self.alloc(doc.len());
        self.write(ptr, doc.as_bytes());
        self.scan_raw(ptr, doc.len() as u32)
    }

    fn scan_raw(&mut self, ptr: u32, len: u32) -> Result<i64, String> {
        let ret = self
            .instance
            .get_typed_func::<(i32, i32), i64>(&self.store, "standalone_wasm_scan")
            .expect("standalone_wasm_scan export")
            .call(&mut self.store, (ptr as i32, len as i32))
            .expect("standalone_wasm_scan call");
        if ret < 0 {
            Err(self
                .last_error()
                .expect("a negative scan must set the error slot"))
        } else {
            Ok(ret)
        }
    }

    fn fill_raw(
        &mut self,
        json_ptr: u32,
        json_len: u32,
        keytab_ptr: u32,
        keytab_len: u32,
    ) -> Result<i32, String> {
        let ret = self
            .instance
            .get_typed_func::<(i32, i32, i32, i32, i32), i32>(
                &self.store,
                "standalone_wasm_fill_lanes",
            )
            .expect("standalone_wasm_fill_lanes export")
            .call(
                &mut self.store,
                (
                    json_ptr as i32,
                    json_len as i32,
                    0,
                    keytab_ptr as i32,
                    keytab_len as i32,
                ),
            )
            .expect("standalone_wasm_fill_lanes call");
        if ret < 0 {
            Err(self
                .last_error()
                .expect("a negative fill must set the error slot"))
        } else {
            Ok(ret)
        }
    }
}

/// SplitMix64, as in `runtime_numerics.rs`: reproducible bit patterns with
/// no dependency.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// ---------------------------------------------------------------------------
// 1. The printer differential.

#[test]
fn the_blob_printer_matches_host_ryu_and_round_trips_bit_exactly() {
    let mut blob = Blob::load();
    let mut values: Vec<f64> = vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.1,
        0.2,
        0.30000000000000004,
        1.0 / 3.0,
        1e-10,
        1e10,
        9007199254740991.0,                    // 2^53 − 1
        f64::from_bits(0x3FF0_0000_0000_0001), // 1 + one ulp
        5e-324,                                // smallest subnormal
        // Bit-identical to the classic 2.2250738585072011e-308.
        2.225_073_858_507_201e-308, // the parser-killer subnormal
        f64::MIN_POSITIVE,
        f64::MAX,
        -f64::MAX,
        std::f64::consts::PI,
        123_456_789.000_000_12, // bit-identical to 123456789.000000123
    ];
    // Random bit patterns, finite ones only: the printer refuses non-finite
    // closes by contract, and that path is covered in `tests/standalone_wasm.rs`.
    let mut state = 0xA2A2_0606_2026_0726u64;
    while values.len() < 20_000 {
        let v = f64::from_bits(splitmix(&mut state));
        if v.is_finite() {
            values.push(v);
        }
    }

    let mut host_ryu = ryu::Buffer::new();
    for (i, &v) in values.iter().enumerate() {
        // Exercise the itoa side with a different i64 each round, signs
        // included.
        let time = (i as i64 - 10) * 715_827_883;
        let got = blob.serialize_one(v, time);
        let expected = format!(
            "{{\"version\":1,\"signals\":[{{\"time\":{time},\"direction\":1,\"price\":{}}}]}}",
            host_ryu.format_finite(v)
        );
        assert_eq!(
            got,
            expected,
            "blob output must match the host build of the same printers for {v:?} ({:#018x})",
            v.to_bits()
        );
        // And the decimal is shortest-roundtrip: parsing it back recovers
        // the exact bits.
        let price = got
            .split("\"price\":")
            .nth(1)
            .and_then(|s| s.strip_suffix("}]}"))
            .expect("price substring");
        let back: f64 = price.parse().expect("price parses as f64");
        assert_eq!(
            back.to_bits(),
            v.to_bits(),
            "parse-back must be bit-exact for {v:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. The scanner differential.

#[test]
fn allocator_overflow_and_invalid_rewind_are_failure_atomic() {
    let mut blob = Blob::load();
    blob.reset();
    let floor = blob.alloc_raw(0);
    assert_ne!(floor, 0);

    assert_eq!(
        blob.alloc_raw(u32::MAX),
        0,
        "an unrepresentable end address must fail"
    );
    assert_eq!(
        blob.alloc_raw(0),
        floor,
        "failed allocation must not advance the arena"
    );

    assert_eq!(
        blob.relocate_raw(u32::MAX - 15),
        -1,
        "a relocation displacement outside the signed ABI must fail"
    );
    assert_eq!(
        blob.alloc_raw(0),
        floor,
        "failed relocation must not move the arena"
    );
    assert!(
        blob.last_error()
            .is_some_and(|error| error.starts_with("memory allocation failed:")),
        "failed relocation must report the allocation error class"
    );

    assert_eq!(blob.alloc_raw(8), floor);
    let top = floor + 8;
    blob.dealloc_raw(u32::MAX - 3, 8);
    assert_eq!(
        blob.alloc_raw(0),
        top,
        "overflowing non-top deallocation must not rewind the arena"
    );
    blob.dealloc_raw(floor, 8);
    assert_eq!(blob.alloc_raw(0), floor, "the exact top allocation rewinds");
}

#[test]
fn the_scanner_rejects_invalid_host_ranges_without_trapping() {
    let mut blob = Blob::load();

    blob.reset();
    assert_eq!(
        blob.scan_raw(0, 1),
        Err("input buffer is outside linear memory".into())
    );

    blob.reset();
    let ptr = blob.alloc(1);
    blob.write(ptr, b"{");
    assert_eq!(
        blob.scan_raw(ptr, 2),
        Err("input buffer is outside the active arena".into())
    );

    blob.reset();
    let ptr = blob.alloc(1);
    assert_eq!(
        blob.scan_raw(ptr, u32::MAX),
        Err("input buffer is outside linear memory".into())
    );

    blob.reset();
    assert_eq!(
        blob.fill_raw(0, 1, 0, 1),
        Err("lane input range is outside linear memory".into())
    );
}

/// A wire-shaped envelope whose `"x"` value is the corpus payload, so the
/// scan/serde comparison isolates the JSON *grammar* from the wire-shape
/// rules (`version`/`params`/`bars` present, bars non-empty and objects)
/// that serde knows nothing of.
fn envelope(value: &str) -> String {
    format!(
        "{{\"version\":1,\"params\":{{}},\
         \"bars\":[{{\"time\":1,\"open\":1.0,\"high\":1.0,\"low\":1.0,\"close\":1.0}}],\
         \"x\":{value}}}"
    )
}

#[test]
fn the_scanner_accepts_everything_serde_accepts() {
    let mut blob = Blob::load();
    let values = [
        // Numbers in every grammatical form.
        "0",
        "-0",
        "7",
        "-7",
        "3.25",
        "0.0001",
        "1e10",
        "1E10",
        "1e+10",
        "1e-10",
        "2.5E-3",
        "1.7976931348623157e308",
        "5e-324",
        "123456789012345678901234567890",
        // Strings, escapes included.
        "\"\"",
        "\"plain\"",
        "\"esc \\\" \\\\ \\/ \\b \\f \\n \\r \\t\"",
        "\"\\u0041\\uD83D\\uDE00\"",
        "\"héllo → ∑\"",
        // Literals and composites.
        "true",
        "false",
        "null",
        "[]",
        "{}",
        "[1,2,3]",
        "[[[[1]]]]",
        "{\"a\":{\"b\":[true,null,\"c\"]}}",
        "[ 1 , {\"k\" : [ ] } ]",
    ];
    for value in values {
        let doc = envelope(value);
        assert!(
            serde_json::from_str::<serde_json::Value>(&doc).is_ok(),
            "corpus bug: serde must accept {value}"
        );
        assert_eq!(
            blob.scan(&doc),
            Ok(1),
            "the scanner must accept what serde accepts: {value}"
        );
    }
    // Insignificant whitespace around the document itself.
    let padded = format!("  \t\n{}\r\n ", envelope("1"));
    assert_eq!(blob.scan(&padded), Ok(1));
}

#[test]
fn the_scanner_rejects_what_serde_rejects_with_the_json_prefix() {
    let mut blob = Blob::load();
    let values = [
        "tru",
        "falsee",
        "nul",
        "+1",
        "01",
        "1.",
        ".5",
        "-",
        "1e",
        "1e+",
        "--1",
        "\"unterminated",
        "\"bad \\q escape\"",
        "\"short \\u00 escape\"",
        "\"ctl \u{1} char\"",
        "[1,]",
        "{\"a\":}",
        "{\"a\" 1}",
        "[1 2]",
        "{1:2}",
        "'single'",
        "{,}",
    ];
    for value in values {
        let doc = envelope(value);
        assert!(
            serde_json::from_str::<serde_json::Value>(&doc).is_err(),
            "corpus bug: serde must reject {value}"
        );
        let error = blob
            .scan(&doc)
            .expect_err(&format!("the scanner must reject {value}"));
        assert!(
            error.starts_with("Invalid JSON: "),
            "{value}: tokenizer failures carry the exact prefix, got {error:?}"
        );
    }
}

#[test]
fn the_three_deliberate_scanner_divergences_hold() {
    let mut blob = Blob::load();

    // Depth: the wire format needs 3 levels; the scanner caps recursion at
    // 32 (bounded shadow stack), serde's default limit is 128. A 40-deep
    // value diverges by design.
    let deep = format!("{}1{}", "[".repeat(40), "]".repeat(40));
    let doc = envelope(&deep);
    assert!(serde_json::from_str::<serde_json::Value>(&doc).is_ok());
    let error = blob.scan(&doc).expect_err("the depth bound must hold");
    assert!(error.contains("nesting"), "got {error:?}");

    // Range: `1e999` is grammatical; the scanner skips unknown values
    // without converting them, serde refuses the out-of-range number. The
    // leniency is one-sided — nothing serde accepts may be refused.
    let doc = envelope("1e999");
    assert!(serde_json::from_str::<serde_json::Value>(&doc).is_err());
    assert_eq!(blob.scan(&doc), Ok(1));

    // Lone surrogates: `\uD800` with no low half. ECMA-404 constrains only
    // the escape's *syntax* (`\u` + four hex digits), which this satisfies,
    // so accepting it is the correct reading — the scanner spans string
    // tokens and never decodes them. serde is stricter because it decodes to
    // a Rust `String`, which cannot hold an unpaired surrogate. The leniency
    // stays one-sided in the same direction as the other two.
    for lone in ["\"\\uD800\"", "\"\\uDFFF tail\"", "\"\\uDC00\\uD800\""] {
        let doc = envelope(lone);
        assert!(
            serde_json::from_str::<serde_json::Value>(&doc).is_err(),
            "corpus bug: serde must reject {lone}"
        );
        assert_eq!(blob.scan(&doc), Ok(1), "the scanner accepts {lone}");
    }
}

// ---------------------------------------------------------------------------
// 3. Splice preconditions the workspace cannot see without this file.

#[test]
fn every_blob_data_segment_stays_at_or_above_4096() {
    // `Runtime::parse` refuses rodata below 4096 (the CSAW header + input
    // table live there), and its failure mode in production is a `OnceLock`
    // panic at first use, not an `Err` — so the bound gets a clean test
    // here, where growing the standalone WebAssembly family's rodata is what moves it.
    let mut segments = 0;
    for payload in wasmparser::Parser::new(0).parse_all(runtime::MODULE_WASM_STANDALONE_WASM) {
        if let wasmparser::Payload::DataSection(reader) = payload.expect("blob parses") {
            for segment in reader {
                let segment = segment.expect("data segment");
                let wasmparser::DataKind::Active {
                    offset_expr,
                    memory_index: 0,
                } = segment.kind
                else {
                    panic!("the blob must use active data segments in memory 0");
                };
                let mut ops = offset_expr.get_operators_reader();
                let wasmparser::Operator::I32Const { value } = ops.read().expect("offset operator")
                else {
                    panic!("blob data offsets must be i32.const");
                };
                assert!(
                    value >= 4096,
                    "data segment at {value} would underrun the reserved low region"
                );
                segments += 1;
            }
        }
    }
    assert!(
        segments > 0,
        "the blob carries rodata; zero segments means a parse bug"
    );
}

#[test]
fn the_runtime_manifests_keep_their_exact_pins() {
    // The checked-in blob is only reproducible if its dependency graph is
    // frozen: `libm` for the numerics (a patch bump can move last-ulp
    // results) and `ryu`/`itoa` for the printers (this file's differential
    // compares against the same-versioned host build). Assert on the
    // checked-in manifests (`=x.y.z` pins make the lockfile redundant here;
    // the runtime's Cargo.lock is gitignored, so a fresh clone has none).
    const RUNTIME_MANIFEST: &str = include_str!("../runtime/Cargo.toml");
    const NUM_MANIFEST: &str = include_str!("../../pinewasm_num/Cargo.toml");
    assert!(
        NUM_MANIFEST.contains("libm = \"=0.2.16\""),
        "libm pin moved"
    );
    assert!(
        RUNTIME_MANIFEST.contains("ryu = { version = \"=1.0.23\""),
        "ryu pin moved"
    );
    assert!(
        RUNTIME_MANIFEST.contains("itoa = { version = \"=1.0.18\""),
        "itoa pin moved"
    );
}
