//! ABI v6 carries a whole transparency percentage in its packed RGBT byte.
//! Admit only producers proved to fit that representation; all other valid
//! colors execute through the interpreter-hosted wrapper with their original values.
use super::*;
use pinewasm_csil::flow::Flow;

pub(super) fn require_representable_transparency(program: &CsilProgram) -> Result<(), AotError> {
    let code = &program.main().code;
    if program.inputs.iter().any(|input| {
        matches!(input.defval, InputValue::Color { t, .. } if t.fract() != 0.0)
    }) || code.iter().any(|instruction| {
        matches!(instruction, CsilInstr::PushColor { t, .. } if t.get().fract() != 0.0)
    }) {
        return Err(AotError::UnsupportedOpcode("fractional color transparency".into()));
    }
    let constructors: Vec<_> = code
        .iter()
        .enumerate()
        .filter_map(|(pc, instruction)| match instruction {
            CsilInstr::CallPure {
                func: PureFunc::ColorNew,
                ..
            } => Some((pc, 1)),
            CsilInstr::CallPure {
                func: PureFunc::ColorRgb,
                ..
            } => Some((pc, 3)),
            CsilInstr::CallPure {
                func: PureFunc::ColorFromGradient,
                ..
            } => Some((pc, 4)),
            _ => None,
        })
        .collect();
    if constructors.is_empty() {
        return Ok(());
    }
    let Some(flow) = Flow::build(program) else {
        return Err(AotError::UnsupportedOpcode(
            "unproven color transparency".into(),
        ));
    };
    for (pc, operand) in constructors {
        if !flow.reached[pc] {
            continue;
        }
        // Gradient interpolation can create a fractional alpha even when its
        // endpoints and numeric arguments are whole. No packed-byte proof is
        // inferred from the source type or from one selected input value.
        let gradient_fits = operand == 4
            && gradient_transparency(pc, program, &flow).is_some_and(|t| t.fract() == 0.0);
        if (operand == 4 && !gradient_fits)
            || (operand != 4
                && (flow.operands[pc][operand].is_empty()
                    || !flow.operands[pc][operand]
                        .iter()
                        .all(|&producer| whole(producer, program, &flow, &mut BTreeSet::new()))))
        {
            return Err(AotError::UnsupportedOpcode(
                "unproven color transparency".into(),
            ));
        }
    }
    Ok(())
}

fn same_known(values: &BTreeSet<usize>, at: impl Fn(usize) -> Option<f64>) -> Option<f64> {
    let first = at(*values.first()?)?;
    values
        .iter()
        .all(|&pc| at(pc) == Some(first))
        .then_some(first)
}

fn number(
    pc: usize,
    program: &CsilProgram,
    flow: &Flow,
    visiting: &mut BTreeSet<usize>,
) -> Option<f64> {
    if !visiting.insert(pc) {
        return None;
    }
    let result = match &program.main().code[pc] {
        CsilInstr::PushI64(value) => Some(*value as f64),
        CsilInstr::PushF64(value) => Some(value.get()),
        CsilInstr::IntToF64 => same_known(&flow.operands[pc][0], |source| {
            number(source, program, flow, &mut visiting.clone())
        }),
        CsilInstr::ReadUserSlot { slot, .. } => same_known(&flow.slots[*slot as usize], |source| {
            number(source, program, flow, &mut visiting.clone())
        }),
        _ => None,
    };
    visiting.remove(&pc);
    result
}

fn fixed_transparency(
    pc: usize,
    program: &CsilProgram,
    flow: &Flow,
    visiting: &mut BTreeSet<usize>,
) -> Option<f64> {
    if !visiting.insert(pc) {
        return None;
    }
    let result = match &program.main().code[pc] {
        CsilInstr::PushColor { t, .. } => Some(t.get()),
        CsilInstr::ReadUserSlot { slot, .. } => same_known(&flow.slots[*slot as usize], |source| {
            fixed_transparency(source, program, flow, &mut visiting.clone())
        }),
        CsilInstr::CallPure {
            func: PureFunc::ColorNew | PureFunc::ColorRgb,
            ..
        } => {
            let operand = if matches!(
                program.main().code[pc],
                CsilInstr::CallPure {
                    func: PureFunc::ColorNew,
                    ..
                }
            ) {
                1
            } else {
                3
            };
            same_known(&flow.operands[pc][operand], |source| {
                number(source, program, flow, &mut BTreeSet::new())
            })
            .map(|t| t.clamp(0.0, 100.0))
        }
        _ => None,
    };
    visiting.remove(&pc);
    result
}

fn gradient_transparency(pc: usize, program: &CsilProgram, flow: &Flow) -> Option<f64> {
    let args = &flow.operands[pc];
    let bottom = same_known(&args[3], |source| {
        fixed_transparency(source, program, flow, &mut BTreeSet::new())
    })?;
    let top = same_known(&args[4], |source| {
        fixed_transparency(source, program, flow, &mut BTreeSet::new())
    })?;
    if bottom == top {
        return Some(bottom);
    }
    let numeric = |index| {
        same_known(&args[index], |source| {
            number(source, program, flow, &mut BTreeSet::new())
        })
    };
    let (value, lower, upper) = (numeric(0)?, numeric(1)?, numeric(2)?);
    let fraction = if upper == lower {
        1.0
    } else {
        ((value - lower) / (upper - lower)).clamp(0.0, 1.0)
    };
    Some(bottom + (top - bottom) * fraction)
}

fn whole(pc: usize, program: &CsilProgram, flow: &Flow, visiting: &mut BTreeSet<usize>) -> bool {
    if !visiting.insert(pc) {
        return false;
    }
    let result = match &program.main().code[pc] {
        CsilInstr::PushNa | CsilInstr::PushI64(_) => true,
        CsilInstr::PushF64(value) => value.get().is_finite() && value.get().fract() == 0.0,
        CsilInstr::ReadInput { input } => program.inputs.get(*input as usize).is_some_and(|decl| {
            matches!(
                decl.kind,
                InputKind::Int | InputKind::Time | InputKind::Enum
            )
        }),
        CsilInstr::ReadUserSlot { slot, .. } => {
            !flow.slots[*slot as usize].is_empty()
                && flow.slots[*slot as usize]
                    .iter()
                    .all(|&source| whole(source, program, flow, visiting))
        }
        CsilInstr::IntToF64
        | CsilInstr::CallPure {
            func: PureFunc::CastFloat,
            argc: 1,
        } => {
            !flow.operands[pc][0].is_empty()
                && flow.operands[pc][0]
                    .iter()
                    .all(|&source| whole(source, program, flow, visiting))
        }
        CsilInstr::CallPure {
            func: PureFunc::CastInt | PureFunc::MathFloor | PureFunc::MathCeil | PureFunc::MathRound,
            argc: 1,
        } => true,
        _ => false,
    };
    visiting.remove(&pc);
    result
}

/// Packed native color cells preserve channels, but not the selected raw
/// input identity. Universal artifacts retain the full Value through the
/// interpreter-hosted tier rather than silently merging equal input colors.
pub(super) fn require_representable_identity(program: &CsilProgram) -> Result<(), AotError> {
    if program
        .main()
        .code
        .iter()
        .any(|instruction| match instruction {
            CsilInstr::ReadInput { input } => program
                .inputs
                .get(*input as usize)
                .is_some_and(|decl| decl.kind == InputKind::Color),
            _ => false,
        })
    {
        return Err(AotError::UnsupportedOpcode("color input provenance".into()));
    }
    Ok(())
}
