//! Production validator prefix; the concrete sink preserves rendering.
use super::{exceeds_offset_space, header_kernel, ValidationError};
use crate::{
    CsilProgram, CSIL_MAGIC, CSIL_VERSION, LANGUAGE_VERSION, MAX_BARS_BACK_DEFAULT,
    MAX_BARS_BACK_LIMIT, RUNTIME_ABI_VERSION,
};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Issue {
    Magic,
    CsilVersion,
    LanguageVersion,
    RuntimeAbiVersion,
    RenderingContractVersion,
    NoFunctions,
    MaxBarsBack,
    OversizedFunction { index: usize, count: usize },
}

#[cfg(test)]
pub(super) trait Sink {
    fn emit(&mut self, issue: Issue);
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Flow {
    Continue,
    Stop,
}

pub(super) trait Emitter<S> {
    fn emit(&self, state: &mut S, issue: Issue);
}

pub(super) struct PrefixContext<'a> {
    pub program: &'a CsilProgram,
}
impl Emitter<Vec<ValidationError>> for PrefixContext<'_> {
    fn emit(&self, errors: &mut Vec<ValidationError>, issue: Issue) {
        let h = &self.program.header;
        errors.push(match issue {
            Issue::Magic => ValidationError::Header(format!(
                "magic is {:?}, expected {CSIL_MAGIC:?}", h.magic)),
            Issue::CsilVersion => ValidationError::Header(format!(
                "CSIL version {} is not supported (expected {CSIL_VERSION})", h.csil_version)),
            Issue::LanguageVersion => ValidationError::Header(format!(
                "source-language version {} is not supported (expected {LANGUAGE_VERSION})", h.language_version)),
            Issue::RuntimeAbiVersion => ValidationError::Header(format!(
                "runtime ABI version {} is not supported (expected {RUNTIME_ABI_VERSION}, {}, or {})",
                h.runtime_abi_version, crate::TA_INPUT_ABI_VERSION, crate::INDEXED_SCAN_ABI_VERSION)),
            Issue::RenderingContractVersion => ValidationError::Metadata(format!(
                "rendering contract version {} is not supported (expected 0 or {})",
                self.program.metadata.rendering_contract_version, crate::RENDERING_CONTRACT_VERSION)),
            Issue::NoFunctions => ValidationError::NoFunctions,
            Issue::MaxBarsBack => ValidationError::Metadata(format!(
                "max_bars_back {} is outside 1..={MAX_BARS_BACK_LIMIT} (default {MAX_BARS_BACK_DEFAULT})",
                self.program.metadata.max_bars_back)),
            Issue::OversizedFunction { index, count } => ValidationError::Structure {
                function: self.program.functions[index].name.clone(),
                message: format!("instruction count {count} exceeds the u32 CSIL offset space"),
            },
        });
    }
}

#[cfg(test)]
pub(super) struct VecSink<'a> {
    pub program: &'a CsilProgram,
    pub errors: &'a mut Vec<ValidationError>,
}
#[cfg(test)]
impl Sink for VecSink<'_> {
    fn emit(&mut self, issue: Issue) {
        PrefixContext {
            program: self.program,
        }
        .emit(self.errors, issue);
    }
}
#[cfg(test)]
struct SinkContext;
#[cfg(test)]
impl<S: Sink> Emitter<S> for SinkContext {
    fn emit(&self, state: &mut S, issue: Issue) {
        state.emit(issue);
    }
}
#[cfg(test)]
pub(super) fn check<S: Sink>(program: &CsilProgram, sink: &mut S) -> Flow {
    check_with_state(program, &SinkContext, sink)
}

/// Immutable inventory scan; rendering only happens after the first finding.
fn first_oversized(program: &CsilProgram) -> Option<(usize, usize)> {
    let mut index = 0;
    while index < program.functions.len() {
        let count = program.functions[index].code.len();
        if exceeds_offset_space(count) {
            return Some((index, count));
        }
        index += 1;
    }
    None
}

pub(super) fn check_with_state<C: Emitter<S>, S>(
    program: &CsilProgram,
    context: &C,
    state: &mut S,
) -> Flow {
    let h = &program.header;
    let issues = header_kernel::check_header(
        header_kernel::HeaderFacts {
            magic: h.magic.as_bytes(),
            csil_version: h.csil_version,
            language_version: h.language_version,
            runtime_abi_version: h.runtime_abi_version,
        },
        CSIL_MAGIC.as_bytes(),
    );
    if issues.magic {
        context.emit(state, Issue::Magic);
    }
    if issues.csil_version {
        context.emit(state, Issue::CsilVersion);
    }
    if issues.language_version {
        context.emit(state, Issue::LanguageVersion);
    }
    if issues.runtime_abi_version {
        context.emit(state, Issue::RuntimeAbiVersion);
    }
    if program.metadata.rendering_contract_version > crate::RENDERING_CONTRACT_VERSION {
        context.emit(state, Issue::RenderingContractVersion);
    }
    if program.functions.is_empty() {
        context.emit(state, Issue::NoFunctions);
        return Flow::Stop;
    }
    if !header_kernel::max_bars_back_admitted(program.metadata.max_bars_back) {
        context.emit(state, Issue::MaxBarsBack);
    }
    if let Some((index, count)) = first_oversized(program) {
        context.emit(state, Issue::OversizedFunction { index, count });
        Flow::Stop
    } else {
        Flow::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn compile() -> CsilProgram {
        let parsed = pinewasm_parse::parse("//@version=6\nindicator(\"prefix\")\nplot(close)");
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analysis = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analysis.diagnostics));
        crate::lower_mir(&pinewasm_mir::lower(&analysis.hir)).expect("lowers")
    }
    // Straight-line copy of the checks in `check_with_state`, used as the baseline for `differential`.
    fn legacy(program: &CsilProgram, errors: &mut Vec<ValidationError>) -> Flow {
        let h = &program.header;
        let header_issues = header_kernel::check_header(
            header_kernel::HeaderFacts {
                magic: h.magic.as_bytes(),
                csil_version: h.csil_version,
                language_version: h.language_version,
                runtime_abi_version: h.runtime_abi_version,
            },
            CSIL_MAGIC.as_bytes(),
        );
        if header_issues.magic {
            errors.push(ValidationError::Header(format!(
                "magic is {:?}, expected {CSIL_MAGIC:?}",
                h.magic
            )));
        }
        if header_issues.csil_version {
            errors.push(ValidationError::Header(format!(
                "CSIL version {} is not supported (expected {CSIL_VERSION})",
                h.csil_version
            )));
        }
        if header_issues.language_version {
            errors.push(ValidationError::Header(format!(
                "source-language version {} is not supported (expected {LANGUAGE_VERSION})",
                h.language_version
            )));
        }
        if header_issues.runtime_abi_version {
            errors.push(ValidationError::Header(format!(
                "runtime ABI version {} is not supported (expected {RUNTIME_ABI_VERSION}, {}, or {})",
                h.runtime_abi_version,
                crate::TA_INPUT_ABI_VERSION, crate::INDEXED_SCAN_ABI_VERSION
            )));
        }
        if program.metadata.rendering_contract_version > 1 {
            errors.push(ValidationError::Metadata(format!(
                "rendering contract version {} is not supported (expected 0 or 1)",
                program.metadata.rendering_contract_version
            )));
        }
        if program.functions.is_empty() {
            errors.push(ValidationError::NoFunctions);
            return Flow::Stop;
        }
        if !header_kernel::max_bars_back_admitted(program.metadata.max_bars_back) {
            errors.push(ValidationError::Metadata(format!(
            "max_bars_back {} is outside 1..={MAX_BARS_BACK_LIMIT} (default {MAX_BARS_BACK_DEFAULT})",
            program.metadata.max_bars_back
        )));
        }

        // Diagnostics and jump operands encode instruction offsets as u32. Reject
        // an oversized in-memory program before any pass could truncate an index
        // while reporting it. A length of u32::MAX + 1 remains representable
        // because its last (zero-based) index is exactly u32::MAX.
        if let Some(func) = program
            .functions
            .iter()
            .find(|func| exceeds_offset_space(func.code.len()))
        {
            errors.push(ValidationError::Structure {
                function: func.name.clone(),
                message: format!(
                    "instruction count {} exceeds the u32 CSIL offset space",
                    func.code.len()
                ),
            });
            return Flow::Stop;
        }

        Flow::Continue
    }
    fn differential(program: &CsilProgram) -> (Flow, Vec<ValidationError>) {
        let mut expected = vec![ValidationError::Header("existing".into())];
        let flow = legacy(program, &mut expected);
        let mut actual = vec![ValidationError::Header("existing".into())];
        let got = check(
            program,
            &mut VecSink {
                program,
                errors: &mut actual,
            },
        );
        assert_eq!(got, flow);
        assert_eq!(actual, expected);
        let mut direct = vec![ValidationError::Header("existing".into())];
        let direct_flow = check_with_state(program, &PrefixContext { program }, &mut direct);
        assert_eq!(direct_flow, flow);
        assert_eq!(direct, expected);
        (got, actual)
    }
    #[test]
    fn valid_and_header_order() {
        let mut p = compile();
        assert_eq!(differential(&p).0, Flow::Continue);
        p.header.magic = "\"wrong\nλ".into();
        p.header.csil_version = u32::MAX;
        p.header.language_version = u32::MAX;
        p.header.runtime_abi_version = u32::MAX;
        p.metadata.max_bars_back = 0;
        let (flow, errors) = differential(&p);
        assert_eq!(flow, Flow::Continue);
        assert_eq!(errors.len(), 6);
        assert!(matches!(errors[1], ValidationError::Header(_)));
        assert!(matches!(errors[5], ValidationError::Metadata(_)));
        p.functions.clear();
        let (flow, errors) = differential(&p);
        assert_eq!(flow, Flow::Stop);
        assert_eq!(errors.len(), 6);
        assert_eq!(errors[5], ValidationError::NoFunctions);
    }
    #[test]
    fn each_header_and_bars_boundary() {
        for arm in 0..4 {
            let mut p = compile();
            match arm {
                0 => p.header.magic.push('\0'),
                1 => p.header.csil_version = u32::MAX,
                2 => p.header.language_version = u32::MAX,
                _ => p.header.runtime_abi_version = u32::MAX,
            }
            assert_eq!(differential(&p).1.len(), 2);
        }
        for bars in [0, 1, MAX_BARS_BACK_LIMIT, MAX_BARS_BACK_LIMIT + 1] {
            let mut p = compile();
            p.metadata.max_bars_back = bars;
            differential(&p);
        }
        let mut p = compile();
        p.header.runtime_abi_version = crate::TA_INPUT_ABI_VERSION;
        assert_eq!(differential(&p).1.len(), 1);
    }
    struct StateEmitter {
        panic_after: Option<usize>,
    }
    impl Emitter<Vec<Issue>> for StateEmitter {
        fn emit(&self, state: &mut Vec<Issue>, issue: Issue) {
            assert_ne!(
                self.panic_after,
                Some(state.len()),
                "injected emission panic"
            );
            state.push(issue);
        }
    }
    #[test]
    fn explicit_state_order_and_panic_prefix() {
        let mut p = compile();
        let mut state = vec![Issue::MaxBarsBack];
        let pointer = state.as_ptr();
        assert_eq!(
            check_with_state(
                &p,
                &StateEmitter {
                    panic_after: Some(1)
                },
                &mut state
            ),
            Flow::Continue
        );
        assert_eq!(state, vec![Issue::MaxBarsBack]);
        assert_eq!(state.as_ptr(), pointer);
        p.header.magic = "bad".into();
        p.header.csil_version = u32::MAX;
        p.functions.clear();
        let mut observed = vec![Issue::MaxBarsBack];
        assert_eq!(
            check_with_state(&p, &StateEmitter { panic_after: None }, &mut observed),
            Flow::Stop
        );
        assert_eq!(
            observed,
            vec![
                Issue::MaxBarsBack,
                Issue::Magic,
                Issue::CsilVersion,
                Issue::NoFunctions
            ]
        );
        let mut partial = vec![Issue::MaxBarsBack];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            check_with_state(
                &p,
                &StateEmitter {
                    panic_after: Some(2),
                },
                &mut partial,
            )
        }));
        assert!(result.is_err());
        assert_eq!(partial, vec![Issue::MaxBarsBack, Issue::Magic]);
    }

    #[derive(Default)]
    struct Tags(Vec<Issue>);
    impl Sink for Tags {
        fn emit(&mut self, issue: Issue) {
            self.0.push(issue);
        }
    }
    #[test]
    fn actual_guard_and_first_stop_without_huge_allocation() {
        let max = usize::try_from(u32::MAX).unwrap();
        let Some(last_length) = max.checked_add(1) else {
            return;
        };
        let Some(oversized) = last_length.checked_add(1) else {
            return;
        };
        let mut sink = Tags::default();
        assert_eq!(first_oversized(&compile()), None);
        for count in [0, 1, max, last_length] {
            assert!(!exceeds_offset_space(count));
        }
        let lengths = [1, last_length, oversized, oversized + 1];
        let mut index = 0;
        let mut stopped = false;
        let mut observed = Vec::new();
        while !stopped && index < lengths.len() {
            observed.push(index);
            if exceeds_offset_space(lengths[index]) {
                sink.emit(Issue::OversizedFunction {
                    index,
                    count: lengths[index],
                });
                stopped = true;
            } else {
                index += 1;
            }
        }
        assert!(stopped);
        assert_eq!(observed, [0, 1, 2]);
        assert_eq!(index, 2);
        assert_eq!(
            sink.0,
            [Issue::OversizedFunction {
                index: 2,
                count: oversized
            }]
        );
        let p = compile();
        let mut errors = Vec::new();
        VecSink {
            program: &p,
            errors: &mut errors,
        }
        .emit(Issue::OversizedFunction {
            index: 0,
            count: oversized,
        });
        assert_eq!(
            errors,
            [ValidationError::Structure {
                function: p.functions[0].name.clone(),
                message: format!("instruction count {oversized} exceeds the u32 CSIL offset space")
            }]
        );
    }
    #[test]
    fn public_callsite_stops_before_later_checks() {
        let mut p = compile();
        p.functions.clear();
        p.metadata.max_bars_back = 0;
        assert_eq!(
            super::super::validate(&p),
            Err(vec![ValidationError::NoFunctions])
        );
    }
}
