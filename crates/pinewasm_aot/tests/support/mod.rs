//! Test support shared by the integration tests that execute emitted
//! standalone WebAssembly modules: the synthetic feed, the wasmi host for a
//! module with no imports, and the IN-document writer.
//!
//! Every suite that runs emitted modules uses this one executor, so they all
//! run modules through the same implementation.

use pinewasm_aot::header;
use pinewasm_vm::MarketData;

pub fn synthetic_data(n: usize) -> MarketData {
    // Deterministic synthetic feed.
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

pub fn read_u32(mem: &[u8], off: u32) -> u32 {
    u32::from_le_bytes(mem[off as usize..off as usize + 4].try_into().unwrap())
}

/// wasmi host for standalone WebAssembly modules: empty linker — a strategy artifact imports
/// nothing, the broker is linked in. Mirrors `tests/standalone_wasm.rs`'s harness.
pub struct StandaloneWasmHost {
    store: wasmi::Store<()>,
    instance: wasmi::Instance,
    memory: wasmi::Memory,
}

impl StandaloneWasmHost {
    pub fn new(wasm: &[u8]) -> StandaloneWasmHost {
        let engine = wasmi::Engine::default();
        let module = wasmi::Module::new(&engine, wasm).expect("wasmi accepts the module");
        let mut store = wasmi::Store::new(&engine, ());
        let instance = <wasmi::Linker<()>>::new(&engine)
            .instantiate_and_start(&mut store, &module)
            .expect("instantiate with an empty linker");
        let memory = instance
            .get_memory(&store, "memory")
            .expect("memory export");
        StandaloneWasmHost {
            store,
            instance,
            memory,
        }
    }

    pub fn evaluate(&mut self, input: &str) -> Result<Vec<u8>, String> {
        let alloc = self
            .instance
            .get_typed_func::<i32, i32>(&self.store, "alloc")
            .expect("alloc export");
        let ptr = alloc
            .call(&mut self.store, input.len() as i32)
            .expect("alloc call") as u32;
        assert_ne!(ptr, 0, "alloc must not fail");
        self.memory.data_mut(&mut self.store)[ptr as usize..ptr as usize + input.len()]
            .copy_from_slice(input.as_bytes());
        let packed = self
            .instance
            .get_typed_func::<(i32, i32), i64>(&self.store, "evaluate_indicator_cabi")
            .expect("evaluate export")
            .call(&mut self.store, (ptr as i32, input.len() as i32))
            .expect("evaluate call") as u64;
        if packed == 0 {
            let packed = self
                .instance
                .get_typed_func::<(), i64>(&self.store, "last_error")
                .expect("last_error export")
                .call(&mut self.store, ())
                .expect("last_error call") as u64;
            assert_ne!(packed, 0, "a 0 return must set last_error");
            let (eptr, elen) = ((packed >> 32) as usize, (packed & 0xFFFF_FFFF) as usize);
            let mem = self.memory.data(&self.store);
            return Err(String::from_utf8(mem[eptr..eptr + elen].to_vec()).expect("UTF-8"));
        }
        let (out_ptr, out_len) = ((packed >> 32) as usize, (packed & 0xFFFF_FFFF) as usize);
        let mem = self.memory.data(&self.store);
        Ok(mem[out_ptr..out_ptr + out_len].to_vec())
    }

    /// Read a plot lane back out of the module's linear memory after a run —
    /// the CSAW header layout that standalone WebAssembly modules keep internally.
    pub fn plot_lane(&mut self, plot: usize) -> Vec<f64> {
        let mem = self.memory.data(&self.store);
        let bars = read_u32(mem, header::OFF_BARS) as usize;
        let base = read_u32(mem, header::OFF_PLOTS_BASE) as usize;
        let start = base + plot * 8 * bars;
        (0..bars)
            .map(|i| f64::from_le_bytes(mem[start + i * 8..start + i * 8 + 8].try_into().unwrap()))
            .collect()
    }
}

/// Serialize a feed into the standalone WebAssembly IN document (shortest-roundtrip floats — the
/// same bits `serde_json` with `float_roundtrip` parses back).
pub fn standalone_wasm_wire(data: &MarketData, fraction: Option<f64>) -> String {
    let mut out = String::from("{\"version\":1,\"params\":{},");
    if let Some(f) = fraction {
        out.push_str(&format!("\"fraction\":{f:?},"));
    }
    out.push_str("\"bars\":[");
    for i in 0..data.bars() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"time\":{},\"open\":{:?},\"high\":{:?},\"low\":{:?},\"close\":{:?},\"volume\":{:?}}}",
            data.time[i], data.open[i], data.high[i], data.low[i], data.close[i], data.volume[i]
        ));
    }
    out.push_str("]}");
    out
}
