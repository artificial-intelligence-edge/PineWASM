//! The dispatch loop of `Vm::run_bar`, as a generic driver over the
//! instruction executor.
//!
//! Each iteration charges one dispatch unit against both instruction
//! ceilings (`vm_charge::charge_counters`), then fetches the instruction at
//! `pc` (running past the end is an error), then executes it; the executor
//! reports whether control falls through, jumps, or returns. The driver is
//! generic over [`DispatchHost`] so Charon/Aeneas can extract it without the
//! VM's `f64`/`String`/heap state: the per-opcode semantics stay in
//! `Vm::exec_instr`, and the loop's charging, fetch and control-flow order is
//! extracted code.

use crate::vm_charge::{charge_counters, ChargeLimit};

/// How an executed instruction continues the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    /// Fall through to `pc + 1`.
    Next,
    /// Continue at the given instruction index.
    Jump(usize),
    /// Leave the loop successfully.
    Return,
}

/// What the dispatch loop needs from the VM.
pub(crate) trait DispatchHost {
    /// One instruction.
    type Instr;
    /// The loop's error type.
    type Error;
    /// The cumulative per-run instruction counter.
    fn remaining(&mut self) -> &mut u64;
    /// The error for an exhausted instruction ceiling at `pc`.
    fn limit_error(&mut self, limit: ChargeLimit, pc: usize) -> Self::Error;
    /// The error for running past the end of the code at `pc`.
    fn past_end(&mut self, pc: usize) -> Self::Error;
    /// Execute one instruction; `budget` is the per-bar counter (for the
    /// "honest metering" extra charges of linear-cost instructions).
    fn exec(
        &mut self,
        instr: &Self::Instr,
        pc: usize,
        budget: &mut u64,
    ) -> Result<Flow, Self::Error>;
}

/// Run `code` from instruction 0 until an instruction returns or an error
/// stops the loop. `budget` is the per-bar instruction counter.
// `match ... { Err(e) => return Err(e) }` instead of `?`: Aeneas's model of
// `?` (the `Try`/`FromResidual` impls) is unverified, so the item avoids it.
#[allow(clippy::question_mark)]
#[inline(always)]
pub(crate) fn dispatch_loop<H: DispatchHost>(
    host: &mut H,
    code: &[H::Instr],
    budget: &mut u64,
) -> Result<(), H::Error> {
    let mut pc: usize = 0;
    loop {
        if let Err(limit) = charge_counters(budget, host.remaining(), 1) {
            return Err(host.limit_error(limit, pc));
        }
        if pc >= code.len() {
            return Err(host.past_end(pc));
        }
        let flow = match host.exec(&code[pc], pc, budget) {
            Ok(flow) => flow,
            Err(error) => return Err(error),
        };
        match flow {
            Flow::Next => pc += 1,
            Flow::Jump(target) => pc = target,
            Flow::Return => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{dispatch_loop, DispatchHost, Flow};
    use crate::vm_charge::{charge_counters, ChargeLimit};

    /// A toy program: each instruction is (extra charge, flow, error?).
    #[derive(Clone, Copy)]
    struct Op {
        extra: u64,
        flow: Flow,
        fail: bool,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Err {
        Limit(ChargeLimit, usize),
        PastEnd(usize),
        Op(usize),
    }

    struct Host {
        remaining: u64,
        trace: Vec<usize>,
    }

    impl DispatchHost for Host {
        type Instr = Op;
        type Error = Err;
        fn remaining(&mut self) -> &mut u64 {
            &mut self.remaining
        }
        fn limit_error(&mut self, limit: ChargeLimit, pc: usize) -> Err {
            Err::Limit(limit, pc)
        }
        fn past_end(&mut self, pc: usize) -> Err {
            Err::PastEnd(pc)
        }
        fn exec(&mut self, op: &Op, pc: usize, budget: &mut u64) -> Result<Flow, Err> {
            self.trace.push(pc);
            if op.extra > 0 {
                if let Err(limit) = charge_counters(budget, &mut self.remaining, op.extra) {
                    return Err(Err::Limit(limit, pc));
                }
            }
            if op.fail {
                return Err(Err::Op(pc));
            }
            Ok(op.flow)
        }
    }

    /// Direct form of `run_bar`'s loop, over the same host, used as the test expectation.
    fn reference(host: &mut Host, code: &[Op], budget: &mut u64) -> Result<(), Err> {
        let mut pc: usize = 0;
        loop {
            match charge_counters(budget, &mut host.remaining, 1) {
                Ok(()) => {}
                Err(limit) => return Err(host.limit_error(limit, pc)),
            }
            let Some(op) = code.get(pc) else {
                return Err(host.past_end(pc));
            };
            match host.exec(op, pc, budget)? {
                Flow::Jump(target) => {
                    pc = target;
                    continue;
                }
                Flow::Return => return Ok(()),
                Flow::Next => {}
            }
            pc += 1;
        }
    }

    #[test]
    fn dispatch_loop_matches_the_reference_loop() {
        // A small deterministic generator over programs, budgets and counters.
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = |m: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % m
        };
        for _ in 0..20_000 {
            let len = next(6) as usize;
            let code: Vec<Op> = (0..len)
                .map(|_| Op {
                    extra: if next(4) == 0 { next(3) } else { 0 },
                    flow: match next(6) {
                        0 => Flow::Return,
                        1 | 2 => Flow::Jump(next(len as u64 + 2) as usize),
                        _ => Flow::Next,
                    },
                    fail: next(15) == 0,
                })
                .collect();
            let budget0 = next(40);
            let remaining0 = next(40);
            let (mut b1, mut b2) = (budget0, budget0);
            let mut h1 = Host {
                remaining: remaining0,
                trace: Vec::new(),
            };
            let mut h2 = Host {
                remaining: remaining0,
                trace: Vec::new(),
            };
            let r1 = dispatch_loop(&mut h1, &code, &mut b1);
            let r2 = reference(&mut h2, &code, &mut b2);
            assert_eq!(r1, r2);
            assert_eq!((b1, h1.remaining, h1.trace), (b2, h2.remaining, h2.trace));
        }
    }
}
