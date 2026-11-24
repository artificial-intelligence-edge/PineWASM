//! Experimental native integer primitives with an explicit validity lane.
//!
//! A pair `(value: i64, valid: i32)` represents every signed integer and `na`.
//! Invalid results use the canonical pair `(0, 0)`; valid MIN is `(MIN, 1)`.
//! Binary arithmetic, negation and comparisons share inline native lowering.
//! Operand-stack validity is partially integrated; stored cells, input/host
//! boundaries and remaining consumers still use sentinel adapters, so the
//! existing sentinel safety gate remains mandatory.

use wasm_encoder::{BlockType, Function, Instruction as I, MemArg, ValType};

#[derive(Clone, Copy)]
pub enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    /// Checked integer quotient, truncated toward zero. Not fractional division.
    Divide,
    Remainder,
}

fn input_validity(f: &mut Function, lhs: u32, rhs: u32, valid: u32) {
    f.instruction(&I::LocalGet(lhs))
        .instruction(&I::I32Eqz)
        .instruction(&I::I32Eqz)
        .instruction(&I::LocalGet(rhs))
        .instruction(&I::I32Eqz)
        .instruction(&I::I32Eqz)
        .instruction(&I::I32And)
        .instruction(&I::LocalSet(valid));
}

fn min_div_minus_one(f: &mut Function, lhs: u32, rhs: u32) {
    f.instruction(&I::LocalGet(lhs))
        .instruction(&I::I64Const(i64::MIN))
        .instruction(&I::I64Eq)
        .instruction(&I::LocalGet(rhs))
        .instruction(&I::I64Const(-1))
        .instruction(&I::I64Eq)
        .instruction(&I::I32And);
}

/// Locals for inline tagged arithmetic. Payload and tag temporaries must
/// be distinct from their respective input locals.
pub(crate) struct BinaryLocals {
    pub lhs: u32,
    pub lhs_valid: u32,
    pub rhs: u32,
    pub rhs_valid: u32,
    pub value: u32,
    pub valid: u32,
}

/// Emit checked arithmetic from locals, leaving `(value, valid)` on the
/// operand stack. This is shared by standalone helpers and native lowering;
/// it neither adds a function call nor changes the caller's local layout.
pub(crate) fn emit_checked_binary(f: &mut Function, op: BinaryOp, l: BinaryLocals) {
    input_validity(f, l.lhs_valid, l.rhs_valid, l.valid);
    f.instruction(&I::LocalGet(l.valid))
        .instruction(&I::If(BlockType::Empty));
    match op {
        BinaryOp::Add | BinaryOp::Subtract => {
            f.instruction(&I::LocalGet(l.lhs))
                .instruction(&I::LocalGet(l.rhs))
                .instruction(match op {
                    BinaryOp::Add => &I::I64Add,
                    _ => &I::I64Sub,
                })
                .instruction(&I::LocalSet(l.value));
            // Signed overflow is detected independently of the result payload.
            f.instruction(&I::LocalGet(l.lhs))
                .instruction(&I::LocalGet(match op {
                    BinaryOp::Add => l.value,
                    _ => l.rhs,
                }))
                .instruction(&I::I64Xor)
                .instruction(&I::LocalGet(match op {
                    BinaryOp::Add => l.rhs,
                    _ => l.lhs,
                }))
                .instruction(&I::LocalGet(l.value))
                .instruction(&I::I64Xor)
                .instruction(&I::I64And)
                .instruction(&I::I64Const(0))
                .instruction(&I::I64LtS)
                .instruction(&I::I32Eqz)
                .instruction(&I::LocalSet(l.valid));
        }
        BinaryOp::Multiply => {
            f.instruction(&I::LocalGet(l.lhs))
                .instruction(&I::LocalGet(l.rhs))
                .instruction(&I::I64Mul)
                .instruction(&I::LocalSet(l.value));
            // Either MIN * -1 ordering overflows and would make the quotient
            // check trap. All other nonzero rhs values can safely divide r.
            min_div_minus_one(f, l.lhs, l.rhs);
            min_div_minus_one(f, l.rhs, l.lhs);
            f.instruction(&I::I32Or)
                .instruction(&I::If(BlockType::Empty))
                .instruction(&I::I32Const(0))
                .instruction(&I::LocalSet(l.valid))
                .instruction(&I::Else)
                .instruction(&I::LocalGet(l.rhs))
                .instruction(&I::I64Eqz)
                .instruction(&I::I32Eqz)
                .instruction(&I::If(BlockType::Empty))
                .instruction(&I::LocalGet(l.value))
                .instruction(&I::LocalGet(l.rhs))
                .instruction(&I::I64DivS)
                .instruction(&I::LocalGet(l.lhs))
                .instruction(&I::I64Eq)
                .instruction(&I::LocalSet(l.valid))
                .instruction(&I::End)
                .instruction(&I::End);
        }
        BinaryOp::Divide | BinaryOp::Remainder => {
            min_div_minus_one(f, l.lhs, l.rhs);
            f.instruction(&I::LocalGet(l.rhs))
                .instruction(&I::I64Eqz)
                .instruction(&I::I32Or)
                .instruction(&I::If(BlockType::Empty))
                .instruction(&I::I32Const(0))
                .instruction(&I::LocalSet(l.valid))
                .instruction(&I::Else)
                .instruction(&I::LocalGet(l.lhs))
                .instruction(&I::LocalGet(l.rhs))
                .instruction(match op {
                    BinaryOp::Divide => &I::I64DivS,
                    _ => &I::I64RemS,
                })
                .instruction(&I::LocalSet(l.value))
                .instruction(&I::End);
        }
    }
    f.instruction(&I::End);
    f.instruction(&I::LocalGet(l.value))
        .instruction(&I::I64Const(0))
        .instruction(&I::LocalGet(l.valid))
        .instruction(&I::Select)
        .instruction(&I::LocalGet(l.valid));
}

/// Parameters: lhs value/tag and rhs value/tag. Results: value/tag.
/// Invalid payloads never trigger arithmetic traps; MIN is not a sentinel.
pub fn checked_binary_function(op: BinaryOp) -> Function {
    let mut f = Function::new([(1, ValType::I64), (1, ValType::I32)]);
    emit_checked_binary(
        &mut f,
        op,
        BinaryLocals {
            lhs: 0,
            lhs_valid: 1,
            rhs: 2,
            rhs_valid: 3,
            value: 4,
            valid: 5,
        },
    );
    f.instruction(&I::End);
    f
}

pub(crate) struct UnaryLocals {
    pub input: u32,
    pub input_valid: u32,
    pub value: u32,
    pub valid: u32,
}

/// Inline checked negation, leaving payload/tag on the stack.
pub(crate) fn emit_checked_negate(f: &mut Function, l: UnaryLocals) {
    f.instruction(&I::LocalGet(l.input_valid))
        .instruction(&I::I32Eqz)
        .instruction(&I::I32Eqz)
        .instruction(&I::LocalGet(l.input))
        .instruction(&I::I64Const(i64::MIN))
        .instruction(&I::I64Ne)
        .instruction(&I::I32And)
        .instruction(&I::LocalSet(l.valid))
        .instruction(&I::I64Const(0))
        .instruction(&I::LocalGet(l.input))
        .instruction(&I::I64Sub)
        .instruction(&I::LocalSet(l.value))
        .instruction(&I::LocalGet(l.value))
        .instruction(&I::I64Const(0))
        .instruction(&I::LocalGet(l.valid))
        .instruction(&I::Select)
        .instruction(&I::LocalGet(l.valid));
}

/// Parameters: value/tag. Result: checked negated value/tag.
pub fn negate_function() -> Function {
    let mut f = Function::new([(1, ValType::I64), (1, ValType::I32)]);
    emit_checked_negate(
        &mut f,
        UnaryLocals {
            input: 0,
            input_valid: 1,
            value: 2,
            valid: 3,
        },
    );
    f.instruction(&I::End);
    f
}

#[derive(Clone, Copy)]
pub enum Comparison {
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

/// Parameters: lhs value/tag, rhs value/tag. Result: native boolean (0/2).
/// Every comparison with a missing operand is false, including not-equal.
pub(crate) fn emit_comparison(
    f: &mut Function,
    op: Comparison,
    lhs: u32,
    lhs_valid: u32,
    rhs: u32,
    rhs_valid: u32,
) {
    f.instruction(&I::LocalGet(lhs_valid))
        .instruction(&I::I32Eqz)
        .instruction(&I::I32Eqz)
        .instruction(&I::LocalGet(rhs_valid))
        .instruction(&I::I32Eqz)
        .instruction(&I::I32Eqz)
        .instruction(&I::I32And)
        .instruction(&I::LocalGet(lhs))
        .instruction(&I::LocalGet(rhs))
        .instruction(match op {
            Comparison::Equal => &I::I64Eq,
            Comparison::NotEqual => &I::I64Ne,
            Comparison::Less => &I::I64LtS,
            Comparison::LessEqual => &I::I64LeS,
            Comparison::Greater => &I::I64GtS,
            Comparison::GreaterEqual => &I::I64GeS,
        })
        .instruction(&I::I32And)
        .instruction(&I::I32Const(2))
        .instruction(&I::I32Mul);
}

pub fn comparison_function(op: Comparison) -> Function {
    let mut f = Function::new([]);
    emit_comparison(&mut f, op, 0, 1, 2, 3);
    f.instruction(&I::End);
    f
}

/// Parameter: f64. Results: truncated integer value/tag. The lower bound
/// includes exact MIN; the upper bound excludes 2^63. NaN/infinities are missing.
pub fn truncate_function() -> Function {
    let mut f = Function::new([(1, ValType::I32)]);
    f.instruction(&I::LocalGet(0))
        .instruction(&I::F64Const((i64::MIN as f64).into()))
        .instruction(&I::F64Ge)
        .instruction(&I::LocalGet(0))
        .instruction(&I::F64Const((i64::MAX as f64).into()))
        .instruction(&I::F64Lt)
        .instruction(&I::I32And)
        .instruction(&I::LocalSet(1))
        .instruction(&I::LocalGet(0))
        .instruction(&I::I64TruncSatF64S)
        .instruction(&I::I64Const(0))
        .instruction(&I::LocalGet(1))
        .instruction(&I::Select)
        .instruction(&I::LocalGet(1))
        .instruction(&I::End);
    f
}

/// Experimental aligned cell layout: eight payload bytes, four tag bytes,
/// four reserved padding bytes. Current native layouts still use eight bytes.
pub const CELL_SIZE: u32 = 16;

fn cell_mem(offset: u64, align: u32) -> MemArg {
    MemArg {
        offset,
        align,
        memory_index: 0,
    }
}

/// Parameter: byte address. Results: exact integer payload and normalized tag.
/// Uninitialized zero-filled cells are missing, rather than valid integer zero.
pub fn load_cell_function() -> Function {
    let mut f = Function::new([]);
    f.instruction(&I::LocalGet(0))
        .instruction(&I::I64Load(cell_mem(0, 3)))
        .instruction(&I::LocalGet(0))
        .instruction(&I::I32Load(cell_mem(8, 2)))
        .instruction(&I::I32Eqz)
        .instruction(&I::I32Eqz)
        .instruction(&I::End);
    f
}

/// Parameters: address, payload, tag. The tag is normalized and a missing
/// payload canonicalized to zero. Check the higher tag address first so an
/// out-of-bounds cell traps before writing its lower payload bytes.
pub fn store_cell_function() -> Function {
    let mut f = Function::new([(1, ValType::I32)]);
    f.instruction(&I::LocalGet(2))
        .instruction(&I::I32Eqz)
        .instruction(&I::I32Eqz)
        .instruction(&I::LocalSet(3))
        .instruction(&I::LocalGet(0))
        .instruction(&I::LocalGet(3))
        .instruction(&I::I32Store(cell_mem(8, 2)))
        .instruction(&I::LocalGet(0))
        .instruction(&I::LocalGet(1))
        .instruction(&I::I64Const(0))
        .instruction(&I::LocalGet(3))
        .instruction(&I::Select)
        .instruction(&I::I64Store(cell_mem(0, 3)))
        .instruction(&I::End);
    f
}

/// Parameters: integer value/tag. Result: f64, NaN only for a missing tag.
pub fn widen_function() -> Function {
    let mut f = Function::new([]);
    f.instruction(&I::LocalGet(1))
        .instruction(&I::If(BlockType::Result(ValType::F64)))
        .instruction(&I::LocalGet(0))
        .instruction(&I::F64ConvertI64S)
        .instruction(&I::Else)
        .instruction(&I::F64Const(f64::NAN.into()))
        .instruction(&I::End)
        .instruction(&I::End);
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_encoder::{
        CodeSection, ExportKind, ExportSection, FunctionSection, MemorySection, MemoryType, Module,
        TypeSection,
    };

    fn instance() -> (wasmi::Store<()>, wasmi::Instance) {
        let mut types = TypeSection::new();
        types.ty().function(
            [ValType::I64, ValType::I32, ValType::I64, ValType::I32],
            [ValType::I64, ValType::I32],
        );
        types
            .ty()
            .function([ValType::I64, ValType::I32], [ValType::F64]);
        types
            .ty()
            .function([ValType::I64, ValType::I32], [ValType::I64, ValType::I32]);
        types.ty().function(
            [ValType::I64, ValType::I32, ValType::I64, ValType::I32],
            [ValType::I32],
        );
        types
            .ty()
            .function([ValType::F64], [ValType::I64, ValType::I32]);
        types
            .ty()
            .function([ValType::I32], [ValType::I64, ValType::I32]);
        types
            .ty()
            .function([ValType::I32, ValType::I64, ValType::I32], []);
        let mut memories = MemorySection::new();
        memories.memory(MemoryType {
            minimum: 1,
            maximum: Some(1),
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        let mut funcs = FunctionSection::new();
        let mut exports = ExportSection::new();
        let mut code = CodeSection::new();
        for (index, (name, ty, body)) in [
            ("add", 0, checked_binary_function(BinaryOp::Add)),
            ("sub", 0, checked_binary_function(BinaryOp::Subtract)),
            ("widen", 1, widen_function()),
            ("mul", 0, checked_binary_function(BinaryOp::Multiply)),
            ("div", 0, checked_binary_function(BinaryOp::Divide)),
            ("rem", 0, checked_binary_function(BinaryOp::Remainder)),
            ("neg", 2, negate_function()),
            ("eq", 3, comparison_function(Comparison::Equal)),
            ("ne", 3, comparison_function(Comparison::NotEqual)),
            ("lt", 3, comparison_function(Comparison::Less)),
            ("le", 3, comparison_function(Comparison::LessEqual)),
            ("gt", 3, comparison_function(Comparison::Greater)),
            ("ge", 3, comparison_function(Comparison::GreaterEqual)),
            ("truncate", 4, truncate_function()),
            ("load", 5, load_cell_function()),
            ("store", 6, store_cell_function()),
        ]
        .into_iter()
        .enumerate()
        {
            funcs.function(ty);
            exports.export(name, ExportKind::Func, index as u32);
            code.function(&body);
        }
        exports.export("memory", ExportKind::Memory, 0);
        let mut m = Module::new();
        m.section(&types)
            .section(&funcs)
            .section(&memories)
            .section(&exports)
            .section(&code);
        let engine = wasmi::Engine::default();
        let module =
            wasmi::Module::new(&engine, &m.finish()[..]).expect("valid tagged integer Wasm");
        let mut store = wasmi::Store::new(&engine, ());
        let instance = wasmi::Linker::new(&engine)
            .instantiate_and_start(&mut store, &module)
            .expect("zero-import tagged primitives");
        (store, instance)
    }

    #[test]
    fn tagged_integer_arithmetic_preserves_min_and_missing_separately() {
        let (mut store, instance) = instance();
        for (export, cases) in [
            (
                "add",
                vec![
                    ((i64::MIN, 1, 0, 1), (i64::MIN, 1)),
                    ((i64::MIN + 1, 1, -1, 1), (i64::MIN, 1)),
                    ((i64::MIN, 1, -1, 1), (0, 0)),
                    ((i64::MAX, 1, 1, 1), (0, 0)),
                    ((i64::MIN, 0, 1, 1), (0, 0)),
                    ((17, 1, 5, 0), (0, 0)),
                    ((-17, 1, 5, 1), (-12, 1)),
                ],
            ),
            (
                "sub",
                vec![
                    ((i64::MIN, 1, 0, 1), (i64::MIN, 1)),
                    ((i64::MIN + 1, 1, 1, 1), (i64::MIN, 1)),
                    ((i64::MIN, 1, 1, 1), (0, 0)),
                    ((i64::MAX, 1, -1, 1), (0, 0)),
                    ((17, 0, 5, 1), (0, 0)),
                    ((-17, 1, 5, 1), (-22, 1)),
                ],
            ),
        ] {
            let function = instance
                .get_typed_func::<(i64, i32, i64, i32), (i64, i32)>(&store, export)
                .unwrap();
            for (input, expected) in cases {
                assert_eq!(
                    function.call(&mut store, input).unwrap(),
                    expected,
                    "{export}: {input:?}"
                );
            }
        }
    }

    #[test]
    fn tagged_integer_widening_does_not_turn_valid_min_into_nan() {
        let (mut store, instance) = instance();
        let widen = instance
            .get_typed_func::<(i64, i32), f64>(&store, "widen")
            .unwrap();
        assert_eq!(
            widen.call(&mut store, (i64::MIN, 1)).unwrap(),
            -9223372036854775808.0
        );
        assert!(widen.call(&mut store, (i64::MIN, 0)).unwrap().is_nan());
        assert!(widen.call(&mut store, (0, 0)).unwrap().is_nan());
        assert_eq!(widen.call(&mut store, (0, 1)).unwrap(), 0.0);
    }

    #[test]
    fn tagged_integer_multiply_quotient_and_remainder_do_not_trap_at_boundaries() {
        let (mut store, instance) = instance();
        for (name, cases) in [
            (
                "mul",
                vec![
                    ((i64::MIN, 1, 1, 1), (i64::MIN, 1)),
                    ((i64::MIN, 1, 0, 1), (0, 1)),
                    ((i64::MIN, 1, -1, 1), (0, 0)),
                    ((-1, 1, i64::MIN, 1), (0, 0)),
                    ((-4611686018427387904, 1, 2, 1), (i64::MIN, 1)),
                    ((i64::MAX, 1, 2, 1), (0, 0)),
                    ((-3, 1, 7, 1), (-21, 1)),
                    ((i64::MIN, 0, -1, 1), (0, 0)),
                ],
            ),
            (
                "div",
                vec![
                    ((i64::MIN, 1, 1, 1), (i64::MIN, 1)),
                    ((i64::MIN, 1, -1, 1), (0, 0)),
                    ((i64::MIN, 1, 0, 1), (0, 0)),
                    ((7, 1, 2, 1), (3, 1)),
                    ((-7, 1, 2, 1), (-3, 1)),
                    ((7, 1, -2, 1), (-3, 1)),
                    ((i64::MIN, 1, 2, 1), (-4611686018427387904, 1)),
                    ((5, 0, 0, 1), (0, 0)),
                ],
            ),
            (
                "rem",
                vec![
                    ((i64::MIN, 1, 1, 1), (0, 1)),
                    ((i64::MIN, 1, -1, 1), (0, 0)),
                    ((i64::MIN, 1, 0, 1), (0, 0)),
                    ((7, 1, 2, 1), (1, 1)),
                    ((-7, 1, 2, 1), (-1, 1)),
                    ((7, 1, -2, 1), (1, 1)),
                    ((7, 1, 0, 0), (0, 0)),
                ],
            ),
        ] {
            let f = instance
                .get_typed_func::<(i64, i32, i64, i32), (i64, i32)>(&store, name)
                .unwrap();
            for (input, expected) in cases {
                assert_eq!(
                    f.call(&mut store, input).unwrap(),
                    expected,
                    "{name}: {input:?}"
                );
            }
        }
    }

    #[test]
    fn tagged_integer_negation_preserves_checked_signed_semantics() {
        let (mut store, instance) = instance();
        let negate = instance
            .get_typed_func::<(i64, i32), (i64, i32)>(&store, "neg")
            .unwrap();
        for (input, expected) in [
            ((i64::MIN, 1), (0, 0)),
            ((i64::MIN + 1, 1), (i64::MAX, 1)),
            ((i64::MAX, 1), (-i64::MAX, 1)),
            ((0, 1), (0, 1)),
            ((i64::MAX, 0), (0, 0)),
        ] {
            assert_eq!(negate.call(&mut store, input).unwrap(), expected);
        }
    }

    #[test]
    fn tagged_integer_comparisons_distinguish_valid_min_and_missing() {
        let (mut store, instance) = instance();
        let names = ["eq", "ne", "lt", "le", "gt", "ge"];
        for (lhs, rhs, expected) in [
            (i64::MIN, i64::MIN, [true, false, false, true, false, true]),
            (i64::MIN, i64::MAX, [false, true, true, true, false, false]),
            (i64::MAX, i64::MIN, [false, true, false, false, true, true]),
            (0, 0, [true, false, false, true, false, true]),
        ] {
            for (name, expected) in names.into_iter().zip(expected) {
                let f = instance
                    .get_typed_func::<(i64, i32, i64, i32), i32>(&store, name)
                    .unwrap();
                assert_eq!(
                    f.call(&mut store, (lhs, 1, rhs, 1)).unwrap(),
                    if expected { 2 } else { 0 },
                    "{name}: {lhs}, {rhs}"
                );
                for (lhs_tag, rhs_tag) in [(0, 1), (1, 0), (0, 0)] {
                    assert_eq!(
                        f.call(&mut store, (lhs, lhs_tag, rhs, rhs_tag)).unwrap(),
                        0,
                        "{name}: missing operand"
                    );
                }
            }
        }
    }

    #[test]
    fn tagged_integer_float_cast_includes_min_and_excludes_nonfinite_or_overflow() {
        let (mut store, instance) = instance();
        let cast = instance
            .get_typed_func::<f64, (i64, i32)>(&store, "truncate")
            .unwrap();
        for (input, expected) in [
            (-9223372036854775808.0, (i64::MIN, 1)),
            (9223372036854774784.0, (9223372036854774784, 1)),
            (9223372036854775808.0, (0, 0)),
            (-9223372036854777856.0, (0, 0)),
            (f64::NAN, (0, 0)),
            (f64::INFINITY, (0, 0)),
            (f64::NEG_INFINITY, (0, 0)),
            (3.9, (3, 1)),
            (-3.9, (-3, 1)),
            (0.0, (0, 1)),
            (-0.0, (0, 1)),
        ] {
            assert_eq!(
                cast.call(&mut store, input).unwrap(),
                expected,
                "cast({input})"
            );
        }
    }

    #[test]
    fn tagged_integer_memory_preserves_min_and_missing_in_adjacent_cells() {
        let (mut state, instance) = instance();
        let load = instance
            .get_typed_func::<i32, (i64, i32)>(&state, "load")
            .unwrap();
        let store = instance
            .get_typed_func::<(i32, i64, i32), ()>(&state, "store")
            .unwrap();
        let base = 32;
        let step = CELL_SIZE as i32;
        assert_eq!(load.call(&mut state, base).unwrap(), (0, 0));
        store.call(&mut state, (base, i64::MIN, 1)).unwrap();
        store.call(&mut state, (base + step, i64::MIN, 0)).unwrap();
        store
            .call(&mut state, (base + step * 2, i64::MAX, 1))
            .unwrap();
        assert_eq!(load.call(&mut state, base).unwrap(), (i64::MIN, 1));
        assert_eq!(load.call(&mut state, base + step).unwrap(), (0, 0));
        assert_eq!(
            load.call(&mut state, base + step * 2).unwrap(),
            (i64::MAX, 1)
        );
        store.call(&mut state, (base, 0, 1)).unwrap();
        assert_eq!(load.call(&mut state, base).unwrap(), (0, 1));
        assert_eq!(load.call(&mut state, base + step).unwrap(), (0, 0));
    }

    #[test]
    fn tagged_integer_memory_rejects_out_of_bounds_before_payload_write() {
        let (mut state, instance) = instance();
        let store = instance
            .get_typed_func::<(i32, i64, i32), ()>(&state, "store")
            .unwrap();
        let memory = instance.get_memory(&state, "memory").unwrap();
        let address = memory.data(&state).len() - 8;
        memory.data_mut(&mut state)[address..].fill(0x5a);
        assert!(store
            .call(&mut state, (address as i32, i64::MIN, 1))
            .is_err());
        assert_eq!(&memory.data(&state)[address..], &[0x5a; 8]);
        assert!(store.call(&mut state, (-1, 1, 1)).is_err());
    }
}
