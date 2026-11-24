use super::{
    compile, data_from_close, floats, run_outcome, tick, CollectionFunc, ElemType, PureFunc,
    RunOptions, RuntimeErrorKind, Value,
};
use crate::RealtimeSim;
use pinewasm_hir::CellAttr;
use pinewasm_mir::{HandleId, HeapObj};

fn retired(identity: u64) -> Value {
    Value::DisplacedTable(std::num::NonZeroU64::new(identity).unwrap())
}

fn table(vm: &mut crate::Vm, position: &str) -> HandleId {
    let type_id = vm
        .program
        .object_types
        .iter()
        .position(|object| object.name == "table")
        .unwrap() as u32;
    let mut fields =
        vec![Value::Na; vm.program.object_types[type_id as usize].field_count as usize];
    fields[crate::DrawKind::Table.field_index("position").unwrap() as usize] =
        Value::String(position.into());
    fields[crate::DrawKind::Table.field_index("columns").unwrap() as usize] = Value::Int(1);
    fields[crate::DrawKind::Table.field_index("rows").unwrap() as usize] = Value::Int(1);
    vm.heap
        .alloc(HeapObj::Object {
            type_id,
            fields,
            table_identity: None,
        })
        .unwrap()
}

fn register(vm: &mut crate::Vm, table: HandleId) {
    vm.stack.push(Value::Ref(table));
    vm.exec_register_drawing(crate::DrawKind::Table, 0, &mut 1_000_000)
        .unwrap();
    assert_eq!(vm.stack.pop(), Some(Value::Ref(table)));
}

fn identity(vm: &crate::Vm, table: HandleId) -> u64 {
    match vm.heap.get(table) {
        Some(HeapObj::Object {
            table_identity: Some(identity),
            ..
        }) => identity.get(),
        other => panic!("registered table identity missing: {other:?}"),
    }
}

#[test]
fn table_lifetime_identity_survives_rollback_and_physical_handle_reuse() {
    let program = compile("indicator(\"Lifetime rollback\")\nvar table t = na\nplot(close)\n");
    let data = data_from_close(&[1.0]);
    let mut vm = crate::Vm::new(&program, &data, &RunOptions::default()).unwrap();
    let snapshot = vm.snapshot(0);
    let first = table(&mut vm, "position.top_left");
    register(&mut vm, first);
    vm.user_slots[0][0] = Value::Ref(first);
    let second = table(&mut vm, "position.top_left");
    register(&mut vm, second);
    let prior_retired = vm.user_slots[0][0].clone();
    assert_eq!(prior_retired, retired(1));
    assert_eq!(prior_retired.as_ref(), None);
    assert_eq!(vm.last_table_identity, 2);

    vm.restore(0, &snapshot).unwrap();
    assert_eq!(vm.last_table_identity, 2);
    let later = table(&mut vm, "position.top_left");
    assert_eq!(later, first, "rollback reuses this physical heap index");
    register(&mut vm, later);
    assert_eq!(identity(&vm, later), 3);
    vm.user_slots[0][0] = Value::Ref(later);
    let newest = table(&mut vm, "position.top_left");
    register(&mut vm, newest);
    assert_eq!(vm.user_slots[0][0], retired(3));
    assert!(!crate::values_equal(&prior_retired, &vm.user_slots[0][0]));
}

#[test]
fn duplicate_registration_keeps_one_lifetime_and_one_live_entry() {
    let program = compile("indicator(\"Repeated registration\")\ntable t = na\nplot(close)\n");
    let data = data_from_close(&[1.0]);
    let mut vm = crate::Vm::new(&program, &data, &RunOptions::default()).unwrap();
    let table = table(&mut vm, "position.top_left");
    register(&mut vm, table);
    register(&mut vm, table);
    assert_eq!(identity(&vm, table), 1);
    assert_eq!(vm.last_table_identity, 1);
    let slot = crate::DrawingRegistry::slot_of(crate::DrawKind::Table).unwrap();
    assert_eq!(vm.drawings.live[slot], vec![table]);
    assert_eq!(vm.drawings.ids[slot].len(), 1);
}

#[test]
fn exhausted_identity_space_fails_before_retiring_or_registering_any_alias() {
    let program = compile("indicator(\"Identity admission\")\ntable t = na\nplot(close)\n");
    let data = data_from_close(&[1.0]);
    let mut vm = crate::Vm::new(&program, &data, &RunOptions::default()).unwrap();
    let first = table(&mut vm, "position.top_left");
    register(&mut vm, first);
    vm.user_slots[0][0] = Value::Ref(first);
    let replacement = table(&mut vm, "position.top_left");
    let heap_before = vm.heap.clone();
    let drawings_before = format!("{:?}", vm.drawings);
    vm.last_table_identity = u64::MAX;
    vm.stack.push(Value::Ref(replacement));
    let error = vm
        .exec_register_drawing(crate::DrawKind::Table, 0, &mut 1_000_000)
        .unwrap_err();
    assert_eq!(error.kind, RuntimeErrorKind::CollectionLimitExceeded);
    assert!(error.message.contains("identity space exhausted"));
    assert_eq!(vm.last_table_identity, u64::MAX);
    assert_eq!(vm.user_slots[0][0], Value::Ref(first));
    assert_eq!(vm.heap, heap_before);
    assert_eq!(format!("{:?}", vm.drawings), drawings_before);
}

#[test]
fn logical_object_copy_clears_lifetime_but_keeps_retired_field_values() {
    let program = compile("indicator(\"Logical copies\")\ntype Holder\n    table id\ntable t = na\nHolder h = Holder.new(t)\nplot(close)\n");
    let data = data_from_close(&[1.0]);
    let mut vm = crate::Vm::new(&program, &data, &RunOptions::default()).unwrap();
    let original = table(&mut vm, "position.top_left");
    register(&mut vm, original);
    let type_id = match vm.heap.get(original).unwrap() {
        HeapObj::Object { type_id, .. } => *type_id,
        _ => unreachable!(),
    };
    vm.stack.push(Value::Ref(original));
    vm.exec_copy_object(type_id, 0, &mut 1_000_000).unwrap();
    let copied = vm.stack.pop().unwrap().as_ref().unwrap();
    assert!(matches!(
        vm.heap.get(copied),
        Some(HeapObj::Object {
            table_identity: None,
            ..
        })
    ));
    register(&mut vm, copied);
    assert_eq!(identity(&vm, copied), 2);

    let holder_type = vm
        .program
        .object_types
        .iter()
        .position(|object| object.name == "Holder")
        .unwrap() as u32;
    let object = vm
        .heap
        .alloc(HeapObj::Object {
            type_id: holder_type,
            fields: vec![retired(1)],
            table_identity: None,
        })
        .unwrap();
    vm.stack.push(Value::Ref(object));
    vm.exec_copy_object(holder_type, 0, &mut 1_000_000).unwrap();
    let copied = vm.stack.pop().unwrap().as_ref().unwrap();
    let Some(HeapObj::Object { fields, .. }) = vm.heap.get(copied) else {
        panic!("copied object")
    };
    assert_eq!(fields, &vec![retired(1)]);
}

#[test]
fn aot_host_state_roundtrip_keeps_the_identity_allocator() {
    let program = std::sync::Arc::new(compile(
        "indicator(\"Host bridge\")\ntable t = na\nplot(close)\n",
    ));
    let options = std::sync::Arc::new(RunOptions::default());
    let data = data_from_close(&[1.0]);
    let mut state = crate::AotVmState::new(&program, data, &options, 1);
    let mut first = crate::Vm::from_aot_state(&program, &options, 1, 0, &mut state);
    first.last_table_identity = 47;
    first.into_aot_state(&mut state);
    let mut second = crate::Vm::from_aot_state(&program, &options, 1, 0, &mut state);
    assert_eq!(second.last_table_identity, 47);
    let table = table(&mut second, "position.top_left");
    register(&mut second, table);
    assert_eq!(identity(&second, table), 48);
}

#[test]
fn only_full_table_cell_distinguishes_displacement_from_literal_na() {
    let program = compile("indicator(\"Receiver policy\")\ntable t = na\nplot(close)\n");
    let data = data_from_close(&[1.0]);
    let mut vm = crate::Vm::new(&program, &data, &RunOptions::default()).unwrap();
    let reservation = vm.bytes_reserved;
    let push = |vm: &mut crate::Vm, receiver: Value, attrs: &[CellAttr]| {
        vm.stack = vec![Value::Int(42), receiver, Value::Int(0), Value::Int(0)];
        vm.stack.extend(std::iter::repeat_n(Value::Na, attrs.len()));
    };
    push(&mut vm, retired(7), CellAttr::all());
    vm.exec_set_cell(CellAttr::all(), 0).unwrap();
    assert_eq!(vm.stack, vec![Value::Int(42)]);
    assert_eq!(vm.bytes_reserved, reservation);
    assert!(vm.drawings.table_cells.is_empty());

    push(&mut vm, Value::Na, CellAttr::all());
    assert_eq!(
        vm.exec_set_cell(CellAttr::all(), 0).unwrap_err().kind,
        RuntimeErrorKind::NaFieldAccess
    );
    for &attr in CellAttr::all() {
        for receiver in [Value::Na, retired(7)] {
            push(&mut vm, receiver, &[attr]);
            assert_eq!(
                vm.exec_set_cell(&[attr], 0).unwrap_err().kind,
                RuntimeErrorKind::NaFieldAccess
            );
            assert_eq!(vm.stack, vec![Value::Int(42)]);
        }
    }
    push(&mut vm, Value::Int(0), CellAttr::all());
    assert_eq!(
        vm.exec_set_cell(CellAttr::all(), 0).unwrap_err().kind,
        RuntimeErrorKind::TypeMismatch
    );
    assert_eq!(vm.bytes_reserved, reservation);
    assert!(vm.drawings.table_cells.is_empty());
}

#[test]
fn displaced_alias_provenance_survives_containers_fields_and_helper_returns() {
    let source = r#"//@version=6
indicator("Displaced alias routes")
type Holder
    table id
var table first = table.new(position.top_left, 1, 1)
var table alias = first
var array<table> ids = array.new_table()
array.push(ids, first)
var map<string, table> saved = map.new<string, table>()
map.put(saved, "old", first)
var Holder holder = Holder.new(first)
f_id(table id) => id
var int[] calls = array.new_int()
f_text() =>
    array.push(calls, 1)
    "ignored direct"
var table replacement = table.new(position.top_left, 1, 1)
table.cell(alias, 0, 0, f_text())
table.cell(array.get(ids, 0), 0, 0, "ignored array")
table.cell(map.get(saved, "old"), 0, 0, "ignored map")
table.cell(holder.id, 0, 0, "ignored field")
table.cell(f_id(first), 0, 0, "ignored helper")
table.cell(replacement, 0, 0, "replacement")
plot(na(alias) ? 1 : 0)
plot(na(array.get(ids, 0)) ? 1 : 0)
plot(na(map.get(saved, "old")) ? 1 : 0)
plot(na(holder.id) ? 1 : 0)
plot(array.size(table.all))
plot(array.size(calls))
"#;
    let outcome = run_outcome(source, &[1.0, 2.0], &RunOptions::default());
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert_eq!(outcome.bars_executed, 2);
    for plot in &outcome.plots[..5] {
        assert_eq!(floats(&plot.values), vec![Some(1.0); 2]);
    }
    assert_eq!(floats(&outcome.plots[5].values), vec![Some(1.0), Some(2.0)]);
    assert_eq!(outcome.drawings.tables.len(), 1);
    assert_eq!(outcome.drawings.tables[0].cells.len(), 1);
    assert_eq!(
        outcome.drawings.tables[0].cells[0].attrs.get("text"),
        Some(&Value::String("replacement".into()))
    );
}

#[test]
fn displacement_keeps_identity_separate_from_na_observations() {
    let program = compile("indicator(\"Na observations\")\ntable t = na\nplot(close)\n");
    let data = data_from_close(&[1.0]);
    let mut vm = crate::Vm::new(&program, &data, &RunOptions::default()).unwrap();
    let mut budget = 100_000;
    assert!(crate::values_equal(&retired(7), &retired(7)));
    assert!(!crate::values_equal(&retired(7), &retired(8)));
    assert!(!crate::values_equal(&retired(7), &Value::Na));
    assert_eq!(
        crate::format::tostring_value(&retired(7), None, 0.01),
        crate::format::tostring_value(&Value::Na, None, 0.01)
    );
    for value in [Value::Na, retired(7)] {
        assert_eq!(
            vm.call_pure(PureFunc::Na, std::slice::from_ref(&value), 0, &mut budget)
                .unwrap(),
            Value::Bool(true)
        );
        vm.stack = vec![value.clone(), value.clone()];
        vm.exec_cmp_eq(false, 0).unwrap();
        assert_eq!(vm.stack, vec![Value::Bool(false)]);
        vm.stack = vec![value.clone()];
        assert_eq!(vm.pop_float(0, "probe").unwrap(), None);
        vm.stack = vec![value.clone()];
        assert_eq!(vm.pop_int(0, "probe").unwrap(), None);
        vm.stack = vec![value];
        assert_eq!(vm.pop_bool(0, "probe").unwrap(), None);
    }
    // Coercion must preserve the marker so a copied table alias still carries
    // its identity and call-specific provenance.
    assert_eq!(
        vm.coerce_element(
            CollectionFunc::ArrayPush,
            ElemType::Object(0),
            retired(7),
            0
        )
        .unwrap(),
        retired(7)
    );
}

#[test]
fn retired_table_calls_on_open_ticks_do_not_break_alias_rollback() {
    let source = r#"//@version=6
indicator("Displaced alias rollback")
var table t = table.new(position.top_left, 1, 1)
var table alias = t
var array<table> saved = array.from(t)
table.cell(t, 0, 0, "bar-start")
if barstate.isrealtime and close == 30
    t := table.new(position.top_left, 1, 1)
    table.cell(alias, 0, 0, "ignored retired direct")
    table.cell(array.get(saved, 0), 0, 0, "ignored retired array")
    table.cell(t, 0, 0, "unconfirmed")
if barstate.isrealtime and close == 31
    table.cell(alias, 0, 0, "restored direct")
    table.cell(array.get(saved, 0), 0, 0, "after-rollback")
plot(na(alias) ? 1 : 0)
plot(na(array.get(saved, 0)) ? 1 : 0)
plot(array.size(table.all))
"#;
    let outcome = run_outcome(
        source,
        &[10.0, 20.0],
        &RunOptions {
            realtime_trace: true,
            realtime: Some(RealtimeSim {
                ticks: vec![tick(2, 30.0, false), tick(2, 31.0, true)],
            }),
            ..RunOptions::default()
        },
    );
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let trace = outcome.realtime_trace.unwrap();
    assert_eq!(
        floats(&trace[0].values),
        vec![Some(1.0), Some(1.0), Some(1.0)]
    );
    assert_eq!(
        floats(&trace[1].values),
        vec![Some(0.0), Some(0.0), Some(1.0)]
    );
    assert_eq!(outcome.drawings.tables.len(), 1);
    assert_eq!(
        outcome.drawings.tables[0].cells[0].attrs.get("text"),
        Some(&Value::String("after-rollback".into()))
    );
}
