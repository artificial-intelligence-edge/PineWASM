//! Integration test for the history-plan fixture through the VM constructor.
//!
//! This is a child of the VM's unit-test module so it can inspect private
//! `Vm` state after calling the real `Vm::new_owned` constructor. It adds no
//! production behavior or test-only hooks to the VM implementation.

use super::data_from_close;
use crate::{RunOptions, RuntimeErrorKind, Vm};
use pinewasm_csil::{validate, CsilInstr, CsilProgram};
use pinewasm_hir::{InputValue, TaInputTarget};
use serde_json::Value as JsonValue;
use std::collections::HashMap;

const SMA_SLOT: u32 = 2;
const CHANGE_SLOT: u32 = 3;
const INPUT_ID: u32 = 0;
const CLOSE: [f64; 9] = [10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0, 90.0];

const SOURCE_PINE: &[u8] = "//@version=6\nindicator(\"History proof\", max_bars_back=2)\nn = input.int(2)\nx = close\nplot(x[2])\nplot(ta.sma(x, n))\nplot(ta.change(x, n))\n".as_bytes();
const PROGRAM_CSIL_JSON: &[u8] = "{\"header\":{\"magic\":\"CSIL\",\"il_version\":2,\"language_version\":6,\"runtime_abi_version\":1},\"constants\":[],\"series_slots\":[{\"name\":\"n\",\"persistent\":false,\"varip\":false},{\"name\":\"x\",\"persistent\":false,\"varip\":false},{\"name\":\"%h0\",\"persistent\":false,\"varip\":false},{\"name\":\"%h1\",\"persistent\":false,\"varip\":false}],\"plots\":[{\"title\":null,\"style\":{\"kind\":\"plot\",\"plotStyle\":\"line\",\"lineStyle\":\"solid\",\"lineWidth\":1,\"display\":\"all\",\"histogramBase\":0.0,\"offset\":0,\"showLast\":null,\"forceOverlay\":false,\"trackPrice\":false,\"join\":false,\"editable\":true,\"format\":null,\"precision\":null,\"group\":null,\"ohlcField\":null,\"markerShape\":null,\"markerLocation\":null,\"markerSize\":null,\"character\":null,\"text\":null,\"minHeight\":null,\"maxHeight\":null,\"unsupported\":[]}},{\"title\":null,\"style\":{\"kind\":\"plot\",\"plotStyle\":\"line\",\"lineStyle\":\"solid\",\"lineWidth\":1,\"display\":\"all\",\"histogramBase\":0.0,\"offset\":0,\"showLast\":null,\"forceOverlay\":false,\"trackPrice\":false,\"join\":false,\"editable\":true,\"format\":null,\"precision\":null,\"group\":null,\"ohlcField\":null,\"markerShape\":null,\"markerLocation\":null,\"markerSize\":null,\"character\":null,\"text\":null,\"minHeight\":null,\"maxHeight\":null,\"unsupported\":[]}},{\"title\":null,\"style\":{\"kind\":\"plot\",\"plotStyle\":\"line\",\"lineStyle\":\"solid\",\"lineWidth\":1,\"display\":\"all\",\"histogramBase\":0.0,\"offset\":0,\"showLast\":null,\"forceOverlay\":false,\"trackPrice\":false,\"join\":false,\"editable\":true,\"format\":null,\"precision\":null,\"group\":null,\"ohlcField\":null,\"markerShape\":null,\"markerLocation\":null,\"markerSize\":null,\"character\":null,\"text\":null,\"minHeight\":null,\"maxHeight\":null,\"unsupported\":[]}}],\"visuals\":[],\"fills\":[],\"ta_states\":[],\"ta_input_bindings\":[{\"input\":0,\"negated\":false,\"target\":{\"Sma\":{\"slot\":2}}},{\"input\":0,\"negated\":false,\"target\":{\"Change\":{\"slot\":3}}}],\"inputs\":[{\"id\":0,\"kind\":\"Int\",\"widget\":\"standard\",\"title\":\"n\",\"tooltip\":null,\"inline\":null,\"group\":null,\"confirm\":false,\"display\":\"all\",\"active\":{\"default\":true,\"dependencies\":[],\"direct\":null,\"expression\":null},\"defval\":{\"Int\":2},\"constraints\":{\"min\":null,\"max\":null,\"step\":null,\"options\":[]},\"span\":{\"source\":0,\"start\":61,\"end\":73}}],\"object_types\":[{\"name\":\"chart.point\",\"field_count\":3},{\"name\":\"line\",\"field_count\":10},{\"name\":\"label\",\"field_count\":14},{\"name\":\"box\",\"field_count\":19},{\"name\":\"table\",\"field_count\":9},{\"name\":\"polyline\",\"field_count\":9},{\"name\":\"linefill\",\"field_count\":3},{\"name\":\"footprint\",\"field_count\":8},{\"name\":\"volume_row\",\"field_count\":8}],\"functions\":[{\"name\":\"main\",\"code\":[{\"ReadInput\":{\"input\":0}},{\"StoreUserSlotCurrent\":{\"slot\":0}},{\"ReadBuiltin\":{\"source\":\"Close\",\"offset\":0}},{\"StoreUserSlotCurrent\":{\"slot\":1}},{\"ReadUserSlot\":{\"slot\":1,\"offset\":2}},{\"PlotF64\":{\"plot_slot\":0}},{\"ReadUserSlot\":{\"slot\":1,\"offset\":0}},{\"StoreUserSlotCurrent\":{\"slot\":2}},{\"WindowMean\":{\"slot\":2,\"length\":2}},{\"PlotF64\":{\"plot_slot\":1}},{\"ReadUserSlot\":{\"slot\":1,\"offset\":0}},{\"StoreUserSlotCurrent\":{\"slot\":3}},{\"ReadUserSlot\":{\"slot\":3,\"offset\":0}},{\"ReadUserSlot\":{\"slot\":3,\"offset\":2}},\"F64Sub\",{\"PlotF64\":{\"plot_slot\":2}},\"Return\"],\"source_map\":[{\"source\":0,\"start\":61,\"end\":73},{\"source\":0,\"start\":57,\"end\":73},{\"source\":0,\"start\":78,\"end\":83},{\"source\":0,\"start\":74,\"end\":83},{\"source\":0,\"start\":89,\"end\":93},{\"source\":0,\"start\":84,\"end\":94},{\"source\":0,\"start\":107,\"end\":108},{\"source\":0,\"start\":100,\"end\":112},{\"source\":0,\"start\":100,\"end\":112},{\"source\":0,\"start\":95,\"end\":113},{\"source\":0,\"start\":129,\"end\":130},{\"source\":0,\"start\":119,\"end\":134},{\"source\":0,\"start\":119,\"end\":134},{\"source\":0,\"start\":119,\"end\":134},{\"source\":0,\"start\":119,\"end\":134},{\"source\":0,\"start\":114,\"end\":135},{\"source\":0,\"start\":114,\"end\":135}],\"max_stack\":2}],\"metadata\":{\"title\":\"History proof\",\"overlay\":false,\"max_bars_back\":2}}".as_bytes();
const PLAN_REQUEST_JSON: &[u8] = "{\"capacity\":8,\"depth\":6,\"format\":\"pw-history-plan-v2\",\"host_inputs\":[{\"input\":0,\"value\":{\"Int\":6}}],\"resolved\":[{\"slot\":2,\"target\":\"sma\",\"value\":6},{\"slot\":3,\"target\":\"change\",\"value\":6}]}".as_bytes();
const PLAN_REQUEST_DEFAULT_JSON: &[u8] = "{\"capacity\":4,\"depth\":2,\"format\":\"pw-history-plan-v2\",\"host_inputs\":[],\"resolved\":[{\"slot\":2,\"target\":\"sma\",\"value\":2},{\"slot\":3,\"target\":\"change\",\"value\":2}]}".as_bytes();

fn sha256_hex(input: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut data = input.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());
    let mut h = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    for block in data.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, x) in w[..16].iter_mut().enumerate() {
            *x = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let a = w[i - 15];
            let b = w[i - 2];
            w[i] = w[i - 16]
                .wrapping_add(a.rotate_right(7) ^ a.rotate_right(18) ^ (a >> 3))
                .wrapping_add(w[i - 7])
                .wrapping_add(b.rotate_right(17) ^ b.rotate_right(19) ^ (b >> 10));
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut z] = h;
        for i in 0..64 {
            let t1 = z
                .wrapping_add(e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25))
                .wrapping_add((e & f) ^ (!e & g))
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let t2 = (a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22))
                .wrapping_add((a & b) ^ (a & c) ^ (b & c));
            z = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, z]) {
            *x = x.wrapping_add(y);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

fn fixture_bytes(name: &str) -> Vec<u8> {
    match name {
        "source.pine" => SOURCE_PINE.to_vec(),
        "program.csil.json" => PROGRAM_CSIL_JSON.to_vec(),
        "plan-request.json" => PLAN_REQUEST_JSON.to_vec(),
        "plan-request-default.json" => PLAN_REQUEST_DEFAULT_JSON.to_vec(),
        other => panic!("unknown fixture {other}"),
    }
}
fn verify_fixture_hashes() {
    let fixtures = [
        (
            "source.pine",
            SOURCE_PINE,
            "955d9bd0e22f55303e7ca632b1dcae99e0aaaa10186c46da778f12f06de5b800",
        ),
        (
            "program.csil.json",
            PROGRAM_CSIL_JSON,
            "884941200e05e5208b0267bec65cc7d15485a720200e584d6efd78a83984c3fc",
        ),
        (
            "plan-request.json",
            PLAN_REQUEST_JSON,
            "16019e1a7f7a53f176b35d5c3e8a6d97f80bdaca58c63c4c6498c5192494d851",
        ),
        (
            "plan-request-default.json",
            PLAN_REQUEST_DEFAULT_JSON,
            "3a07f0a5334aa33c176b65e54723401d241ac84084b76e9878066c2f5ecf1862",
        ),
    ];
    for (name, bytes, expected) in fixtures {
        assert_eq!(sha256_hex(bytes), expected, "pinned {name} SHA-256");
    }
}

fn admitted_program() -> CsilProgram {
    let bytes = fixture_bytes("program.csil.json");
    let json = std::str::from_utf8(&bytes).expect("fixture CSIL is UTF-8");
    let program = CsilProgram::from_json(json).expect("decode checked-in CSIL artifact");
    validate(&program).expect("checked-in artifact validates");
    program
}

fn parse_plan_request(bytes: &[u8]) -> Result<JsonValue, String> {
    let request: JsonValue = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if request.get("format").and_then(JsonValue::as_str) != Some("pw-history-plan-v2") {
        return Err("unexpected plan-request format".to_string());
    }
    if !request.get("host_inputs").is_some_and(JsonValue::is_array)
        || !request.get("resolved").is_some_and(JsonValue::is_array)
    {
        return Err("plan request is missing host_inputs or resolved entries".to_string());
    }
    Ok(request)
}

fn plan_request(name: &str) -> JsonValue {
    let bytes = fixture_bytes(name);
    parse_plan_request(&bytes).unwrap_or_else(|error| panic!("decode {name}: {error}"))
}

fn request_options(request: &JsonValue) -> RunOptions {
    let mut options = RunOptions::default();
    for host in request["host_inputs"]
        .as_array()
        .expect("request host_inputs array")
    {
        let id = host["input"].as_u64().expect("host input id") as u32;
        let int = host["value"]["Int"]
            .as_i64()
            .expect("fixture host override is an integer");
        options.inputs.insert(id, InputValue::Int(int));
    }
    options
}

fn request_integer(request: &JsonValue, field: &str) -> u32 {
    request[field]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_else(|| panic!("request field {field} must be a u32"))
}

fn resolved_value(request: &JsonValue, target: &str, slot: u32) -> u32 {
    request["resolved"]
        .as_array()
        .expect("request resolved array")
        .iter()
        .find(|entry| {
            entry["target"].as_str() == Some(target)
                && entry["slot"].as_u64() == Some(u64::from(slot))
        })
        .and_then(|entry| entry["value"].as_u64())
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_else(|| panic!("request lacks resolved {target} value for slot {slot}"))
}

fn resolved_map(program: &CsilProgram, options: &RunOptions) -> HashMap<TaInputTarget, i64> {
    program
        .ta_input_bindings
        .iter()
        .map(|binding| {
            let value = program
                .resolve_ta_input(binding, options.inputs.get(&binding.input))
                .unwrap_or_else(|| panic!("Rust resolver rejected binding {binding:?}"));
            (binding.target, value)
        })
        .collect()
}

fn specialized_window_and_change(vm: &Vm) -> (u32, u32) {
    let mut window = None;
    let mut change = None;
    for instruction in &vm.program.main().code {
        match instruction {
            CsilInstr::WindowMean {
                slot: SMA_SLOT,
                length,
            } => window = Some(*length),
            CsilInstr::ReadUserSlot {
                slot: CHANGE_SLOT,
                offset,
            } if *offset != 0 => change = Some(*offset),
            _ => {}
        }
    }
    (
        window.expect("specialized WindowMean operand"),
        change.expect("specialized nonzero Change read operand"),
    )
}

fn physical_history_read(vm: &Vm, slot: u32, bar: usize, offset: usize) -> &crate::Value {
    assert!(bar >= offset, "reads only initialized historical bars");
    &vm.user_slots[slot as usize][(bar - offset) & vm.slot_mask]
}

fn run_and_check_fixture_case(request_name: &str, expected: u32) {
    let request = plan_request(request_name);
    let program = admitted_program();
    let options = request_options(&request);
    let sma = resolved_value(&request, "sma", SMA_SLOT);
    let change = resolved_value(&request, "change", CHANGE_SLOT);
    let expected_depth = request_integer(&request, "depth");
    let expected_capacity = request_integer(&request, "capacity");

    assert_eq!(sma, expected);
    assert_eq!(change, expected);
    let rust_resolved = resolved_map(&program, &options);
    assert_eq!(
        rust_resolved.get(&TaInputTarget::Sma { slot: SMA_SLOT }),
        Some(&i64::from(expected))
    );
    assert_eq!(
        rust_resolved.get(&TaInputTarget::Change { slot: CHANGE_SLOT }),
        Some(&i64::from(expected))
    );
    assert_eq!(
        program.history_depth_with_ta_inputs(&rust_resolved),
        expected_depth,
        "Rust planner result from this artifact and resolved map"
    );

    let data = data_from_close(&CLOSE);
    let mut vm = Vm::new_owned(&program, None, &data, &options)
        .expect("actual VM constructor accepts admitted fixture and inputs");
    assert_eq!(specialized_window_and_change(&vm), (sma, change));
    assert_eq!(vm.program.history_depth(), expected_depth);
    assert_eq!(vm.slot_mask + 1, expected_capacity as usize);
    assert!(vm
        .user_slots
        .iter()
        .all(|slot| slot.len() == expected_capacity as usize));

    let (bars, errors) = vm.run_history();
    assert!(errors.is_empty(), "VM history execution errors: {errors:?}");
    assert_eq!(bars, CLOSE.len() as u32);

    let final_bar = CLOSE.len() - 1;
    let expected_sma =
        CLOSE[CLOSE.len() - expected as usize..].iter().sum::<f64>() / f64::from(expected);
    let expected_change = CLOSE[final_bar] - CLOSE[final_bar - expected as usize];
    assert_eq!(vm.plots[1][final_bar], crate::Value::Float(expected_sma));
    assert_eq!(vm.plots[2][final_bar], crate::Value::Float(expected_change));

    for offset in 0..expected as usize {
        assert_eq!(
            physical_history_read(&vm, SMA_SLOT, final_bar, offset),
            &crate::Value::Float(CLOSE[final_bar - offset]),
            "SMA ring sample at offset {offset}"
        );
    }
    assert_eq!(
        physical_history_read(&vm, CHANGE_SLOT, final_bar, expected as usize),
        &crate::Value::Float(CLOSE[final_bar - expected as usize]),
        "Change ring sample at the resolved offset"
    );

    eprintln!(
        "{request_name}: resolved SMA/Change={expected}, Rust depth={expected_depth}, allocated ring={expected_capacity}, final SMA={expected_sma}, final Change={expected_change}"
    );
}

#[test]
fn vm_constructor_bridge_plan() {
    verify_fixture_hashes();
    let source = fixture_bytes("source.pine");
    assert!(std::str::from_utf8(&source)
        .unwrap()
        .contains("ta.sma(x, n)"));

    // The request fixtures are constant inputs to this test.
    run_and_check_fixture_case("plan-request.json", 6);
    run_and_check_fixture_case("plan-request-default.json", 2);

    let program = admitted_program();
    let data = data_from_close(&CLOSE);

    // A wrong-kind value falls back to the declaration default.
    let mut wrong_kind = RunOptions::default();
    wrong_kind.inputs.insert(INPUT_ID, InputValue::Bool(false));
    let wrong_kind_vm = Vm::new_owned(&program, None, &data, &wrong_kind)
        .expect("wrong-kind input uses declaration default");
    assert_eq!(specialized_window_and_change(&wrong_kind_vm), (2, 2));
    assert_eq!(wrong_kind_vm.slot_mask + 1, 4);

    // Invalid SMA length is rejected before a VM with history storage exists.
    for invalid_length in [0, 100_001] {
        let mut invalid = RunOptions::default();
        invalid
            .inputs
            .insert(INPUT_ID, InputValue::Int(invalid_length));
        let invalid_result = Vm::new_owned(&program, None, &data, &invalid);
        let invalid_error = match invalid_result {
            Ok(_) => panic!("out-of-range SMA length {invalid_length} must be rejected"),
            Err(error) => error,
        };
        assert_eq!(invalid_error.kind, RuntimeErrorKind::InvalidTaLength);
    }

    // Exercise the actual constructor on both sides of the 8-cell power-of-2
    // boundary: depth seven needs eight cells; depth eight needs sixteen.
    for (length, capacity) in [(7, 8), (8, 16)] {
        let mut options = RunOptions::default();
        options.inputs.insert(INPUT_ID, InputValue::Int(length));
        let vm = Vm::new_owned(&program, None, &data, &options)
            .expect("boundary TA input remains in the admitted range");
        let values = resolved_map(&program, &options);
        assert_eq!(program.history_depth_with_ta_inputs(&values), length as u32);
        assert_eq!(vm.program.history_depth(), length as u32);
        assert_eq!(vm.slot_mask + 1, capacity as usize);
    }

    // The test adapter rejects malformed request bytes and malformed CSIL;
    // neither parser is meant to be used outside this test.
    assert!(parse_plan_request(b"{not-json").is_err());
    assert!(parse_plan_request(br#"{"format":"pw-history-plan-v2"}"#).is_err());
    assert!(CsilProgram::from_json("{").is_err());
    let mut invalid_artifact = program;
    let window = invalid_artifact
        .main()
        .code
        .iter()
        .position(|instruction| matches!(instruction, CsilInstr::WindowMean { .. }))
        .expect("fixture contains WindowMean");
    let CsilInstr::WindowMean { length, .. } = &mut invalid_artifact.functions[0].code[window]
    else {
        unreachable!();
    };
    *length = 0;
    assert!(validate(&invalid_artifact).is_err());
}
