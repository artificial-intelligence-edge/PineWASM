//! Maintainer tool: dump every builtin function signature the registry knows,
//! one per line. The stable, sorted text format makes registry changes easy to
//! review as a diff of signature lines.
//!
//! Run from the repository root:
//!
//! ```text
//! cargo run -p pinewasm_semantics --example dump_registry
//! ```
//!
//! Format: `name<TAB>kind<TAB>ret<TAB>p1;p2;...<TAB>variadic` where each
//! parameter is `name:type:qualifier:required`. Engine-backed `ta.*` builtins
//! carry their `ta_engine` signature instead of `params`; the collection and
//! drawing families are checked by dedicated analyzers and so report no
//! parameters here. This is an inspection tool, not a release gate; automated
//! coverage remains owned by the matrix and compiler tests.

use pinewasm_semantics::{BuiltinRegistry, TaParamKind};

fn main() {
    let registry = BuiltinRegistry::standard();
    let mut lines: Vec<String> = Vec::new();

    for f in registry.functions() {
        let params: Vec<String> = match &f.ta_engine {
            Some(engine) => engine
                .iter()
                .map(|p| {
                    let ty = match p.kind {
                        TaParamKind::Source => "source",
                        TaParamKind::SourceDefaulted(_) => "source_defaulted",
                        TaParamKind::Condition => "condition",
                        TaParamKind::ConstInt { .. } => "const_int",
                        TaParamKind::ConstFloat { .. } => "const_float",
                        TaParamKind::ConstPercent => "const_percent",
                        TaParamKind::ConstBool { .. } => "const_bool",
                        TaParamKind::ConstTimeframe { .. } => "const_timeframe",
                    };
                    format!("{}:{}:engine:{}", p.name, ty, p.required())
                })
                .collect(),
            None => f
                .params
                .iter()
                .map(|p| format!("{}:{:?}:{:?}:{}", p.name, p.ty, p.max_qualifier, p.required))
                .collect(),
        };
        // The kind's `Debug` payload (a `TaFunc`, a `ScalarTy`, ...) is noise
        // for a signature diff; keep the leading variant name only.
        let kind = format!("{:?}", f.kind);
        let kind = kind.split(['(', ' ', '{']).next().unwrap_or(&kind);
        let variadic = f
            .variadic_max
            .map(|m| format!("variadic_max={m}"))
            .unwrap_or_default();
        lines.push(format!(
            "{}\t{}\t{:?}\t{}\t{}",
            f.name,
            kind,
            f.ret,
            params.join(";"),
            variadic
        ));
    }

    lines.sort();
    for line in lines {
        println!("{line}");
    }
}
