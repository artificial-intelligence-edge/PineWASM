//! Human-readable CSIL listing, for debug tabs and snapshot tests.

use crate::{CsilInstr, CsilProgram, InputValue};
use std::fmt::Write as _;

/// Render an [`InputValue`] for the CSIL listing (stable across snapshots).
fn render_input_value(value: &InputValue) -> String {
    match value {
        InputValue::Int(v) => v.to_string(),
        InputValue::Float(v) => format!("{v}"),
        InputValue::Bool(v) => v.to_string(),
        InputValue::String(v) => format!("{v:?}"),
        InputValue::Color { r, g, b, t } => {
            if *t == 0.0 {
                format!("#{r:02X}{g:02X}{b:02X}")
            } else {
                format!("#{r:02X}{g:02X}{b:02X} t={t}")
            }
        }
        InputValue::Source(series) => series.name().to_string(),
        InputValue::ExternalSource(source) => format!(
            "external({:?}, slot {}, {:?})",
            source.script_id, source.plot_slot, source.plot_label
        ),
        InputValue::Enum {
            name,
            index,
            member,
            title,
        } => {
            let identity = if member.is_empty() {
                format!("{name}#{index}")
            } else {
                format!("{name}.{member}#{index}")
            };
            match title {
                Some(title) => format!("{identity} title={title:?}"),
                None => identity,
            }
        }
    }
}

pub fn dump_csil(program: &CsilProgram) -> String {
    let mut out = String::new();
    let h = &program.header;
    let _ = writeln!(
        out,
        "{} version {} lang {} abi {}",
        h.magic, h.csil_version, h.language_version, h.runtime_abi_version
    );
    if let Some(title) = &program.metadata.title {
        let _ = writeln!(out, "PROGRAM {title:?}");
    }
    for (i, c) in program.constants.iter().enumerate() {
        let _ = writeln!(out, "CONST {i} {c:?}");
    }
    for (i, slot) in program.series_slots.iter().enumerate() {
        let suffix = if slot.varip {
            " varip"
        } else if slot.persistent {
            " var"
        } else {
            ""
        };
        let _ = writeln!(out, "SERIES_SLOT {i} {:?}{suffix}", slot.name);
    }
    for (i, plot) in program.plots.iter().enumerate() {
        match &plot.title {
            Some(t) => {
                let _ = writeln!(out, "PLOT_SLOT {i} {t:?}");
            }
            None => {
                let _ = writeln!(out, "PLOT_SLOT {i}");
            }
        }
    }
    for (i, decl) in program.ta_states.iter().enumerate() {
        let _ = writeln!(out, "TA_STATE {i} {} {:?}", decl.func.name(), decl.params);
    }
    for decl in &program.inputs {
        let _ = writeln!(
            out,
            "INPUT {} input.{} title={:?} defval={}",
            decl.id,
            decl.kind.describe(),
            decl.title.as_deref().unwrap_or("<none>"),
            render_input_value(&decl.defval),
        );
    }
    for func in &program.functions {
        let _ = writeln!(out, "\nFUNC {} (max_stack {})", func.name, func.max_stack);
        for (i, instr) in func.code.iter().enumerate() {
            // Spans into the main buffer print bare (`38..43`); spans into an
            // imported library carry their source id (`1:38..43`) so hosts can
            // attribute inlined instructions to the right buffer.
            let span = func.source_map.get(i).map_or_else(
                || "?".to_string(),
                |s| {
                    if s.source.0 == 0 {
                        format!("{}..{}", s.start, s.end)
                    } else {
                        format!("{}:{}..{}", s.source.0, s.start, s.end)
                    }
                },
            );
            let _ = writeln!(out, "  {i:04} {:<32} ; {span}", render_instr(instr));
        }
        let _ = writeln!(out, "END");
    }
    out
}

fn render_instr(instr: &CsilInstr) -> String {
    match instr {
        CsilInstr::PushNa => "PUSH_NA".into(),
        CsilInstr::PushBool(b) => format!("PUSH_BOOL {b}"),
        CsilInstr::PushI64(v) => format!("PUSH_I64 {v}"),
        CsilInstr::PushF64(v) => format!("PUSH_F64 {v}"),
        CsilInstr::PushStr(id) => format!("PUSH_STR const{id}"),
        CsilInstr::PushColor { r, g, b, t } => {
            if t.get() == 0.0 {
                format!("PUSH_COLOR #{r:02X}{g:02X}{b:02X}")
            } else {
                format!("PUSH_COLOR #{r:02X}{g:02X}{b:02X} t={t}")
            }
        }
        CsilInstr::Dup => "DUP".into(),
        CsilInstr::Pop => "POP".into(),
        CsilInstr::ReadBuiltin { source, offset } => {
            format!("READ_BUILTIN {} {offset}", source.name().to_uppercase())
        }
        CsilInstr::ReadUserSlot { slot, offset } => format!("READ_USER_SLOT {slot} {offset}"),
        CsilInstr::ReadBuiltinDynamic { source } => {
            format!("READ_BUILTIN_DYNAMIC {}", source.name().to_uppercase())
        }
        CsilInstr::ReadUserSlotDynamic { slot } => format!("READ_USER_SLOT_DYNAMIC {slot}"),
        CsilInstr::ReadInput { input } => format!("READ_INPUT {input}"),
        CsilInstr::ReadSecurity {
            selector,
            source,
            htf_seconds,
            calendar_months,
            gaps_on,
            lookahead_on,
            ..
        } => {
            let prefix = selector
                .as_ref()
                .map(|s| format!("{s} "))
                .unwrap_or_default();
            format!(
                "READ_SECURITY {prefix}{} {htf_seconds} months={calendar_months:?} gaps={} lookahead={}",
                source.name().to_uppercase(),
                if *gaps_on { "on" } else { "off" },
                if *lookahead_on { "on" } else { "off" },
            )
        }
        CsilInstr::ReadSecurityExpr {
            expr,
            htf_seconds,
            calendar_months,
            gaps_on,
            lookahead_on,
            ..
        } => format!(
            "READ_SECURITY_EXPR {} {htf_seconds} months={calendar_months:?} gaps={} lookahead={}",
            htf_expr_text(expr),
            if *gaps_on { "on" } else { "off" },
            if *lookahead_on { "on" } else { "off" },
        ),
        CsilInstr::ReadRequest { func, selector } => {
            format!("READ_REQUEST {} {selector}", func.member())
        }
        CsilInstr::BuildFootprint { selector } => format!("BUILD_FOOTPRINT {selector}"),
        CsilInstr::ReadSecurityLowerTf { selector } => {
            format!("READ_SECURITY_LOWER_TF {selector}")
        }
        CsilInstr::ReadPivotLevels {
            system,
            anchor_seconds,
        } => {
            format!("READ_PIVOT_LEVELS {} {anchor_seconds}", system.type_name())
        }
        CsilInstr::CallPivotPointLevels { argc } => format!("CALL_PIVOT_POINT_LEVELS {argc}"),
        CsilInstr::ReadTimeTf { close, tf_seconds } => {
            format!(
                "READ_TIME_TF {} {tf_seconds}",
                if *close { "close" } else { "open" }
            )
        }
        CsilInstr::FootprintRowByPrice => "FOOTPRINT_ROW_BY_PRICE".to_string(),
        CsilInstr::StoreUserSlotCurrent { slot } => format!("STORE_USER_SLOT_CURRENT {slot}"),
        CsilInstr::WindowMean { slot, length } => format!("WINDOW_MEAN {slot} {length}"),
        CsilInstr::WindowMeanDynamic { slot } => format!("WINDOW_MEAN_DYNAMIC {slot}"),
        CsilInstr::WindowExtremumDynamic { slot, highest } => {
            let name = if *highest { "HIGHEST" } else { "LOWEST" };
            format!("WINDOW_{name}_DYNAMIC {slot}")
        }
        CsilInstr::WindowStatisticDynamic { slot, func } => {
            format!("WINDOW_{}_DYNAMIC {slot}", func.name().to_ascii_uppercase())
        }
        CsilInstr::EmaSimpleDynamic { slot } => format!("EMA_SIMPLE_DYNAMIC {slot}"),
        CsilInstr::CallPure { func, argc } => format!("CALL_PURE {} {argc}", func.name()),
        CsilInstr::CallCollection { func, argc } => {
            format!("CALL_COLLECTION {} {argc}", func.name())
        }
        CsilInstr::CallTa { state } => format!("CALL_TA {state}"),
        CsilInstr::IntAdd => "INT_ADD".into(),
        CsilInstr::IntSub => "INT_SUB".into(),
        CsilInstr::IntMul => "INT_MUL".into(),
        CsilInstr::IntDiv => "INT_DIV".into(),
        CsilInstr::PineIntDiv => "PINE_INT_DIV".into(),
        CsilInstr::IntRem => "INT_REM".into(),
        CsilInstr::IntNeg => "INT_NEG".into(),
        CsilInstr::F64Add => "F64_ADD".into(),
        CsilInstr::F64Sub => "F64_SUB".into(),
        CsilInstr::F64Mul => "F64_MUL".into(),
        CsilInstr::F64Div => "F64_DIV".into(),
        CsilInstr::F64Rem => "F64_REM".into(),
        CsilInstr::F64Neg => "F64_NEG".into(),
        CsilInstr::IntToF64 => "INT_TO_F64".into(),
        CsilInstr::StrConcat => "STR_CONCAT".into(),
        CsilInstr::Select => "SELECT".into(),
        CsilInstr::CmpGt => "CMP_GT".into(),
        CsilInstr::CmpGe => "CMP_GE".into(),
        CsilInstr::CmpLt => "CMP_LT".into(),
        CsilInstr::CmpLe => "CMP_LE".into(),
        CsilInstr::CmpEq => "CMP_EQ".into(),
        CsilInstr::CmpNe => "CMP_NE".into(),
        CsilInstr::BoolAnd => "BOOL_AND".into(),
        CsilInstr::BoolOr => "BOOL_OR".into(),
        CsilInstr::BoolNot => "BOOL_NOT".into(),
        CsilInstr::Jump { target } => format!("JUMP {target:04}"),
        CsilInstr::JumpIfFalse { target } => format!("JUMP_IF_FALSE {target:04}"),
        CsilInstr::JumpIfExact { on: true, target } => format!("JUMP_IF_EXACT_TRUE {target:04}"),
        CsilInstr::JumpIfExact { on: false, target } => format!("JUMP_IF_EXACT_FALSE {target:04}"),
        CsilInstr::JumpIfSlotWritten { slot, target } => {
            format!("JUMP_IF_SLOT_WRITTEN {slot} {target:04}")
        }
        CsilInstr::PlotF64 { plot_slot } => format!("PLOT_F64 {plot_slot}"),
        CsilInstr::PlotStyledF64 {
            plot_slot,
            color_count,
        } => format!("PLOT_STYLED_F64 {plot_slot} {color_count}"),
        CsilInstr::VisualColor {
            visual_slot,
            color_count,
        } => format!("VISUAL_COLOR {visual_slot} {color_count}"),
        CsilInstr::FillColor {
            fill_slot,
            color_count,
        } => format!("FILL_COLOR {fill_slot} {color_count}"),
        CsilInstr::RuntimeError => "RUNTIME_ERROR".into(),
        CsilInstr::NewObject {
            type_id,
            field_count,
        } => format!("NEW_OBJECT {type_id} {field_count}"),
        CsilInstr::CopyObject { type_id } => format!("COPY_OBJECT {type_id}"),
        CsilInstr::GetField { field_idx } => format!("GET_FIELD {field_idx}"),
        CsilInstr::SetField { field_idx } => format!("SET_FIELD {field_idx}"),
        CsilInstr::RegisterDrawing { kind } => format!("REGISTER_DRAWING {}", kind.text_tag()),
        CsilInstr::ReadDrawingsAll { kind } => format!("READ_DRAWINGS_ALL {}", kind.text_tag()),
        CsilInstr::DeleteDrawing { kind } => format!("DELETE_DRAWING {}", kind.text_tag()),
        CsilInstr::SetCell { attrs } => {
            let tags = attrs
                .iter()
                .map(|a| a.text_tag())
                .collect::<Vec<_>>()
                .join(",");
            format!("SET_CELL {tags}")
        }
        CsilInstr::ClearCells => "CLEAR_CELLS".into(),
        CsilInstr::MergeCells => "MERGE_CELLS".into(),
        CsilInstr::Log { level, argc } => format!("LOG {} {argc}", level.text_tag()),
        CsilInstr::Alert => "ALERT".into(),
        CsilInstr::AlertCondition => "ALERT_CONDITION".into(),
        CsilInstr::Order { kind } => format!("ORDER strategy.{}", kind.member()),
        CsilInstr::Return => "RETURN".into(),
    }
}

/// Render a compound `request.security` [`HtfExpr`] tree in a compact,
/// parenthesized prefix form for the human-readable CSIL dump.
fn htf_expr_text(expr: &pinewasm_hir::HtfExpr) -> String {
    use pinewasm_hir::{BinOp, HtfExpr};
    match expr {
        HtfExpr::BoolResult(inner) => format!("BOOL_RESULT({})", htf_expr_text(inner)),
        HtfExpr::Const(v) => format!("{v}"),
        HtfExpr::Source(s) => s.name().to_uppercase(),
        HtfExpr::InputSource { input } => format!("INPUT_SOURCE {input}"),
        HtfExpr::BarstateIslast => "BARSTATE_ISLAST".to_owned(),
        HtfExpr::BarstateIsRealtime => "BARSTATE_ISREALTIME".to_owned(),
        HtfExpr::History { offset, source, .. } => {
            format!("(history {offset} {})", htf_expr_text(source))
        }
        HtfExpr::HistoryInput { offset, source, .. } => {
            format!(
                "(history_input {} {})",
                offset.selector_fragment(),
                htf_expr_text(source)
            )
        }
        HtfExpr::HistoryRealtime {
            history_offset,
            realtime_offset,
            source,
            ..
        } => format!(
            "(history_realtime historical={history_offset} realtime={realtime_offset} {})",
            htf_expr_text(source)
        ),
        HtfExpr::EnumSelect {
            input,
            index,
            when_equal,
            otherwise,
        } => format!(
            "(enum input{input} == {index} {} {})",
            htf_expr_text(when_equal),
            otherwise
                .as_deref()
                .map_or_else(|| "na".into(), htf_expr_text)
        ),
        HtfExpr::Neg(inner) => format!("(- {})", htf_expr_text(inner)),
        HtfExpr::Binary {
            op,
            int_result,
            lhs,
            rhs,
        } => {
            // Mark integer-typed `/`/`%` (truncating) distinctly from float
            // division in the debug text so the two are not confused.
            let sym = if *int_result && matches!(op, BinOp::Div | BinOp::Rem) {
                match op {
                    BinOp::Div => "//",
                    _ => "%%",
                }
            } else {
                op.symbol()
            };
            format!("({} {} {})", sym, htf_expr_text(lhs), htf_expr_text(rhs))
        }
        HtfExpr::Ta {
            func,
            length,
            source,
            ..
        } => format!("({} {length} {})", func.name(), htf_expr_text(source)),
        HtfExpr::TaInput {
            func,
            input,
            source,
            ..
        } => {
            format!("({} input{input} {})", func.name(), htf_expr_text(source))
        }
    }
}
