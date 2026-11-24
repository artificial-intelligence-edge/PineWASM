//! Slot-based SMA uses a dense observation clock, separate from bar history.
use super::{mem_f64, mem_u32_at, BodyEmitter, VTy};
use crate::emit::SMA_META_BYTES;
use wasm_encoder::{BlockType, Instruction as I, MemArg, ValType};

fn float_at(offset: u32) -> MemArg {
    MemArg {
        offset: u64::from(offset),
        ..mem_f64()
    }
}

impl BodyEmitter<'_, '_> {
    /// Address the dense observation cell, with the state's base in t32a.
    fn sma_cell_addr(&mut self, observation: u32, mask: u32) {
        self.f
            .instruction(&I::LocalGet(self.ctx.locals.t32a))
            .instruction(&I::LocalGet(observation))
            .instruction(&I::I32Const(mask as i32))
            .instruction(&I::I32And)
            .instruction(&I::I32Const(3))
            .instruction(&I::I32Shl)
            .instruction(&I::I32Add);
    }

    pub(super) fn emit_window_mean(&mut self, slot: u32, length: u32) {
        let l = self.ctx.locals;
        let state = self.ctx.slots.sma(slot);
        self.emit_ta_parameter(pinewasm_hir::TaInputTarget::Sma { slot }, i64::from(length));
        self.f
            .instruction(&I::LocalSet(l.ti0))
            .instruction(&I::GlobalGet(self.ctx.globals.slot_global(slot)))
            .instruction(&I::I32Const(state.relative_offset as i32))
            .instruction(&I::I32Add)
            .instruction(&I::LocalTee(l.t32a))
            .instruction(&I::I32Load(mem_u32_at(4)))
            .instruction(&I::LocalSet(l.t32b))
            .instruction(&I::LocalGet(l.t32a))
            .instruction(&I::I32Load(mem_u32_at(0)))
            .instruction(&I::LocalGet(l.bar))
            .instruction(&I::I32Const(1))
            .instruction(&I::I32Add)
            .instruction(&I::I32Eq)
            .instruction(&I::If(BlockType::Empty));
        // Undo the preceding evaluation on this bar before replacing its
        // source. A loop never creates several historical observations.
        self.f
            .instruction(&I::LocalGet(l.t32a))
            .instruction(&I::I32Load(mem_u32_at(8)))
            .instruction(&I::LocalTee(l.t32c))
            .instruction(&I::LocalGet(l.t32b))
            .instruction(&I::I32Ne)
            .instruction(&I::If(BlockType::Empty));
        self.sma_cell_addr(l.t32c, state.mask);
        self.f
            .instruction(&I::LocalGet(l.t32a))
            .instruction(&I::F64Load(float_at(16)))
            .instruction(&I::F64Store(float_at(SMA_META_BYTES)))
            .instruction(&I::End)
            .instruction(&I::LocalGet(l.t32c))
            .instruction(&I::LocalSet(l.t32b))
            .instruction(&I::Else)
            .instruction(&I::LocalGet(l.t32a))
            .instruction(&I::LocalGet(l.t32b))
            .instruction(&I::I32Store(mem_u32_at(8)))
            .instruction(&I::LocalGet(l.t32a))
            .instruction(&I::LocalGet(l.bar))
            .instruction(&I::I32Const(1))
            .instruction(&I::I32Add)
            .instruction(&I::I32Store(mem_u32_at(0)))
            .instruction(&I::End);

        // Append only present source values. Save the overwritten cell so
        // a second evaluation on this bar can restore the prior history.
        self.f
            .instruction(&l.cur_get(slot))
            .instruction(&I::LocalTee(l.tf0))
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::F64Eq)
            .instruction(&I::If(BlockType::Empty))
            .instruction(&I::LocalGet(l.t32a));
        self.sma_cell_addr(l.t32b, state.mask);
        self.f
            .instruction(&I::F64Load(float_at(SMA_META_BYTES)))
            .instruction(&I::F64Store(float_at(16)));
        self.sma_cell_addr(l.t32b, state.mask);
        self.f
            .instruction(&I::LocalGet(l.tf0))
            .instruction(&I::F64Store(float_at(SMA_META_BYTES)))
            .instruction(&I::LocalGet(l.t32b))
            .instruction(&I::I32Const(1))
            .instruction(&I::I32Add)
            .instruction(&I::LocalSet(l.t32b))
            .instruction(&I::End)
            .instruction(&I::LocalGet(l.t32a))
            .instruction(&I::LocalGet(l.t32b))
            .instruction(&I::I32Store(mem_u32_at(4)))
            .instruction(&I::LocalGet(l.t32b))
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::I32WrapI64)
            .instruction(&I::I32LtU)
            .instruction(&I::If(BlockType::Result(ValType::F64)))
            .instruction(&I::F64Const(f64::NAN.into()))
            .instruction(&I::Else)
            .instruction(&I::F64Const(0.0.into()))
            .instruction(&I::LocalSet(l.wm_sum))
            .instruction(&I::LocalGet(l.t32b))
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::I32WrapI64)
            .instruction(&I::I32Sub)
            .instruction(&I::LocalSet(l.wm_i))
            .instruction(&I::Loop(BlockType::Empty));
        // Sum oldest to newest in exactly the interpreter's arithmetic order.
        self.sma_cell_addr(l.wm_i, state.mask);
        self.f
            .instruction(&I::F64Load(float_at(SMA_META_BYTES)))
            .instruction(&I::LocalGet(l.wm_sum))
            .instruction(&I::F64Add)
            .instruction(&I::LocalSet(l.wm_sum))
            .instruction(&I::LocalGet(l.wm_i))
            .instruction(&I::I32Const(1))
            .instruction(&I::I32Add)
            .instruction(&I::LocalTee(l.wm_i))
            .instruction(&I::LocalGet(l.t32b))
            .instruction(&I::I32LtU)
            .instruction(&I::BrIf(0))
            .instruction(&I::End)
            .instruction(&I::LocalGet(l.wm_sum))
            .instruction(&I::LocalGet(l.ti0))
            .instruction(&I::F64ConvertI64S)
            .instruction(&I::F64Div);
        self.canonicalize_f64();
        self.f.instruction(&I::End);
        self.pushed(VTy::F64);
    }
}
