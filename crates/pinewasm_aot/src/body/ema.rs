//! Run-time-length EMA (`EMA_SIMPLE_DYNAMIC`): one seed-then-recurse smoother
//! per call site, in the arithmetic order of `pinewasm_ta::Smoother::step`.
use super::{mem_f64, mem_u32_at, BodyEmitter, VTy};
use crate::status;
use wasm_encoder::{BlockType, Instruction as I, MemArg, ValType};

/// Offsets inside a site's [`crate::emit::EMA_META_BYTES`] of state.
const SUM: u32 = 0;
const VALUE: u32 = 8;
const COUNT: u32 = 16;
const LENGTH: u32 = 20;

fn float_at(offset: u32) -> MemArg {
    MemArg {
        offset: u64::from(offset),
        ..mem_f64()
    }
}

impl BodyEmitter<'_, '_> {
    /// Pop the length and the source, advance the site's smoother, and push
    /// its value. The interpreter is the specification
    /// (`CsilInstr::EmaSimpleDynamic` in `pinewasm_vm`):
    ///
    /// * a length outside `1..=MAX_WINDOW_LENGTH` (an `na` one included), or
    ///   one that differs from the length the site first ran with, stops the
    ///   run;
    /// * an `na` source yields `na` and leaves the smoother untouched;
    /// * otherwise the smoother sums the first `length` sources, seeds with
    ///   their mean, and then recurses with `alpha = 2 / (length + 1)`.
    ///
    /// The site is seeded exactly when its count has reached its length: the
    /// count stops there, and the length cannot change during a run.
    pub(super) fn emit_ema_simple_dynamic(&mut self, pc: usize, slot: u32) {
        let l = self.ctx.locals;
        let state = self.ctx.slots.ema(slot);
        let max = i64::from(pinewasm_csil::MAX_WINDOW_LENGTH);

        self.want(2);
        self.consumed();
        self.f.instruction(&I::LocalSet(l.ti0));
        self.consumed();
        self.f.instruction(&I::LocalSet(l.tf0));

        // The integer `na` sentinel is `i64::MIN`, so the lower bound
        // rejects it with every other length below one.
        self.f
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::I64Const(1))
            .instruction(&I::I64LtS)
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::I64Const(max))
            .instruction(&I::I64GtS)
            .instruction(&I::I32Or)
            .instruction(&I::If(BlockType::Empty));
        self.err_exit(status::INVALID_TA_LENGTH, pc as u32);
        self.f.instruction(&I::End);

        // The length the site first ran with is zero until then, and no
        // legal length is zero.
        self.f
            .instruction(&I::GlobalGet(self.ctx.globals.slot_global(slot)))
            .instruction(&I::I32Const(state.relative_offset as i32))
            .instruction(&I::I32Add)
            .instruction(&I::LocalTee(l.t32a))
            .instruction(&I::I32Load(mem_u32_at(LENGTH)))
            .instruction(&I::LocalTee(l.t32b))
            .instruction(&I::I32Const(0))
            .instruction(&I::I32Ne)
            .instruction(&I::LocalGet(l.t32b))
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::I32WrapI64)
            .instruction(&I::I32Ne)
            .instruction(&I::I32And)
            .instruction(&I::If(BlockType::Empty));
        self.err_exit(status::INVALID_TA_LENGTH, pc as u32);
        self.f
            .instruction(&I::End)
            .instruction(&I::LocalGet(l.t32a))
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::I32WrapI64)
            .instruction(&I::I32Store(mem_u32_at(LENGTH)));

        // `x - x` is zero for a finite source only: `na` is NaN here.
        self.f
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::F64Sub)
            .instruction(&I::F64Const(0.0.into()))
            .instruction(&I::F64Eq)
            .instruction(&I::If(BlockType::Result(ValType::F64)));
        {
            self.f
                .instruction(&I::LocalGet(l.t32a))
                .instruction(&I::I32Load(mem_u32_at(COUNT)))
                .instruction(&I::LocalTee(l.t32b))
                .instruction(&I::LocalGet(l.ti0))
                .instruction(&I::I32WrapI64)
                .instruction(&I::I32GeU)
                .instruction(&I::If(BlockType::Result(ValType::F64)));
            // Seeded: `alpha * x + (1 - alpha) * value`.
            self.f
                .instruction(&I::F64Const(2.0.into()))
                .instruction(&I::LocalGet(l.ti0))
                .instruction(&I::F64ConvertI64S)
                .instruction(&I::F64Const(1.0.into()))
                .instruction(&I::F64Add)
                .instruction(&I::F64Div)
                .instruction(&I::LocalSet(l.tf1))
                .instruction(&I::LocalGet(l.t32a))
                .instruction(&I::LocalGet(l.tf1))
                .instruction(&I::LocalGet(l.tf0))
                .instruction(&I::F64Mul)
                .instruction(&I::F64Const(1.0.into()))
                .instruction(&I::LocalGet(l.tf1))
                .instruction(&I::F64Sub)
                .instruction(&I::LocalGet(l.t32a))
                .instruction(&I::F64Load(float_at(VALUE)))
                .instruction(&I::F64Mul)
                .instruction(&I::F64Add)
                .instruction(&I::LocalTee(l.tf1))
                .instruction(&I::F64Store(float_at(VALUE)))
                .instruction(&I::LocalGet(l.tf1))
                .instruction(&I::Else);
            // Still seeding: add the source and count it.
            self.f
                .instruction(&I::LocalGet(l.t32a))
                .instruction(&I::LocalGet(l.t32a))
                .instruction(&I::F64Load(float_at(SUM)))
                .instruction(&I::LocalGet(l.tf0))
                .instruction(&I::F64Add)
                .instruction(&I::LocalTee(l.tf1))
                .instruction(&I::F64Store(float_at(SUM)))
                .instruction(&I::LocalGet(l.t32a))
                .instruction(&I::LocalGet(l.t32b))
                .instruction(&I::I32Const(1))
                .instruction(&I::I32Add)
                .instruction(&I::LocalTee(l.t32b))
                .instruction(&I::I32Store(mem_u32_at(COUNT)))
                .instruction(&I::LocalGet(l.t32b))
                .instruction(&I::LocalGet(l.ti0))
                .instruction(&I::I32WrapI64)
                .instruction(&I::I32GeU)
                .instruction(&I::If(BlockType::Result(ValType::F64)))
                .instruction(&I::LocalGet(l.t32a))
                .instruction(&I::LocalGet(l.tf1))
                .instruction(&I::LocalGet(l.ti0))
                .instruction(&I::F64ConvertI64S)
                .instruction(&I::F64Div)
                .instruction(&I::LocalTee(l.tf1))
                .instruction(&I::F64Store(float_at(VALUE)))
                .instruction(&I::LocalGet(l.tf1))
                .instruction(&I::Else)
                .instruction(&I::F64Const(f64::NAN.into()))
                .instruction(&I::End)
                .instruction(&I::End);
        }
        self.f
            .instruction(&I::Else)
            .instruction(&I::F64Const(f64::NAN.into()))
            .instruction(&I::End);
        // A value that overflowed to infinity is `na`, as `Value::from_f64`
        // makes it.
        self.canonicalize_f64();
        self.pushed(VTy::F64);
    }
}
