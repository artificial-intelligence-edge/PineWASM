//! Sweep over user-function call arity.
//!
//! Every program in the sweep defines one user function `f` with up to four
//! parameters (every default pattern) and calls it once with an ordered
//! argument list (positional arguments, then named ones): 7411 programs.
//! Each program contributes one line `id<TAB>class<TAB>outcome`, where
//! `outcome` is a digest of the HIR dump and diagnostics, or `panic`. Instead
//! of checking in all 7411 lines, this module keeps their compact form:
//! [`BASELINE_CASES`], [`BASELINE_OMITTED`] (the 167 programs of the `omitted`
//! class) and [`BASELINE_DIGEST`], the FNV-1a digest of the 7244 non-`omitted`
//! lines in sweep order. The test checks that numeric HIR and diagnostics are
//! unchanged byte for byte (same digest) and that every `omitted` program
//! reports `E0224` instead of panicking.
//! Default plot colour metadata is excluded from this arity digest; the render
//! tests cover it. The complete HIR remains in `run()` for binder differential
//! tests.
//!
//! Each line also carries the program's kernel class: `bind-err` when
//! [`binder::bind`] rejects the call, otherwise the result of
//! [`binder::plan_def_args`] on the dense argument count (`ok`, `count` or
//! `omitted`).
//!
//! To see which program changed after a digest mismatch, write the
//! per-program lines of two builds and diff them:
//! `PINEWASM_DEF_ARITY_DUMP=/path/to/lines.tsv cargo test -p
//! pinewasm_semantics def_arity_sweep`.

use crate::binder::{self, ArgShape, ArityErr};
use pinewasm_hir::dump_hir;
use pinewasm_span::{codes, Diagnostic};
use std::panic::{catch_unwind, AssertUnwindSafe};

const PARAMS: [&str; 4] = ["a", "b", "c", "d"];
const UNKNOWN: &str = "zz";
/// An undefined name with no did-you-mean candidate. A name with several
/// equally close candidates (`u`: `na`/`nz`) gets a suggestion that depends on
/// `HashMap` iteration order, which would make the digests nondeterministic.
const BAD_ARG: &str = "qqqqqqqq";
/// Number of generated programs.
const BASELINE_CASES: usize = 7411;
/// Programs of the `omitted` class, which report E0224.
const BASELINE_OMITTED: usize = 167;
/// FNV-1a digest of the non-`omitted` lines
/// (`id<TAB>class<TAB>outcome<LF>`, in sweep order).
const BASELINE_DIGEST: u64 = 0x0bce_deb4_814c_57fc;

/// One generated program.
pub(crate) struct Case {
    /// `signature|call`, unique per case.
    pub(crate) id: String,
    pub(crate) source: String,
    pub(crate) class: Class,
    /// Whether the first positional argument is the undefined [`BAD_ARG`].
    pub(crate) bad_first: bool,
    pub(crate) param_names: Vec<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    BindErr,
    Ok,
    Count,
    /// [`ArityErr::OmittedWithoutDefault`] for this parameter index.
    Omitted(usize),
}

impl Class {
    fn label(self) -> String {
        match self {
            Class::BindErr => "bind-err".to_string(),
            Class::Ok => "ok".to_string(),
            Class::Count => "count".to_string(),
            Class::Omitted(i) => format!("omitted:{i}"),
        }
    }
}

/// Every ordered named-argument suffix of length <= 2 over the parameter
/// names plus one unknown name (repeats included, so duplicates occur).
fn named_suffixes(names: &[&'static str]) -> Vec<Vec<&'static str>> {
    let mut alphabet: Vec<&'static str> = names.to_vec();
    alphabet.push(UNKNOWN);
    let mut out: Vec<Vec<&'static str>> = vec![vec![]];
    for x in &alphabet {
        out.push(vec![*x]);
    }
    for x in &alphabet {
        for y in &alphabet {
            out.push(vec![*x, *y]);
        }
    }
    out
}

fn classify(names: &[&str], has_default: &[bool], n_pos: usize, named: &[&str]) -> Class {
    let mut shapes: Vec<ArgShape> = vec![ArgShape::Positional; n_pos];
    for name in named {
        shapes.push(ArgShape::Named(names.iter().position(|p| p == name)));
    }
    match binder::bind(names.len(), &shapes, has_default) {
        Err(_) => Class::BindErr,
        Ok(dense) => match binder::plan_def_args(dense.len(), has_default) {
            Ok(_) => Class::Ok,
            Err(ArityErr::Count { .. }) => Class::Count,
            Err(ArityErr::OmittedWithoutDefault { param }) => Class::Omitted(param),
        },
    }
}

/// The whole sweep, in a fixed order.
pub(crate) fn cases() -> Vec<Case> {
    let mut out = Vec::new();
    for n in 0..=PARAMS.len() {
        let names = &PARAMS[..n];
        for bits in 0..(1u32 << n) {
            let has_default: Vec<bool> = (0..n).map(|i| bits >> i & 1 == 1).collect();
            let params: Vec<String> = (0..n)
                .map(|i| {
                    if has_default[i] {
                        format!("{} = {}.5", names[i], i + 1)
                    } else {
                        names[i].to_string()
                    }
                })
                .collect();
            let body = if n == 0 {
                "0.0".to_string()
            } else {
                names.join(" + ")
            };
            let signature = format!("f({})", params.join(", "));
            for n_pos in 0..=n + 1 {
                for bad_first in [false, true] {
                    if bad_first && n_pos == 0 {
                        continue;
                    }
                    for named in named_suffixes(names) {
                        let mut args: Vec<String> = (0..n_pos)
                            .map(|k| {
                                if k == 0 && bad_first {
                                    BAD_ARG.to_string()
                                } else {
                                    format!("{}0.0", k + 1)
                                }
                            })
                            .collect();
                        for (k, name) in named.iter().enumerate() {
                            args.push(format!("{name} = {}00.0", k + 1));
                        }
                        let call = format!("f({})", args.join(", "));
                        let source = format!(
                            "//@version=6\nindicator(\"t\")\n{signature} => {body}\nplot({call})\n"
                        );
                        out.push(Case {
                            id: format!("{signature}|{call}"),
                            source,
                            class: classify(names, &has_default, n_pos, &named),
                            bad_first,
                            param_names: names.to_vec(),
                        });
                    }
                }
            }
        }
    }
    out
}

/// FNV-1a, 64-bit: stable across Rust versions, unlike `DefaultHasher`.
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Parse and analyze `source`; `None` when the analyzer panics.
pub(crate) fn run(source: &str) -> Option<(String, Vec<Diagnostic>)> {
    let parsed = pinewasm_parse::parse(source);
    assert!(
        !pinewasm_span::has_errors(&parsed.diagnostics),
        "sweep source must parse cleanly: {source}\n{:?}",
        parsed.diagnostics
    );
    catch_unwind(AssertUnwindSafe(|| {
        let result = crate::analyze(&parsed.program);
        (dump_hir(&result.hir), result.diagnostics)
    }))
    .ok()
}

pub(crate) fn outcome(result: Option<&(String, Vec<Diagnostic>)>) -> String {
    match result {
        None => "panic".to_string(),
        Some((dump, diagnostics)) => {
            format!("ok:{:016x}", fnv1a(&format!("{dump}\n{diagnostics:?}")))
        }
    }
}

#[test]
fn def_arity_sweep_matches_baseline() {
    let cases = cases();
    let mut rendered = String::new();
    let mut unchanged = String::new();
    let mut omitted = 0usize;
    for case in &cases {
        let result = run(&case.source);
        // These sources contain one numeric plot and no colour argument, so
        // default paint is left out of the arity digest; the render tests
        // check paint separately.
        let arity_result = result.as_ref().map(|(dump, diagnostics)| {
            (
                dump.replace(
                    "    Color Color\n      ConstColor #2196F3 : const color\n",
                    "",
                ),
                diagnostics.clone(),
            )
        });
        let outcome = outcome(arity_result.as_ref());
        let line = format!("{}\t{}\t{}\n", case.id, case.class.label(), outcome);
        rendered.push_str(&line);
        match case.class {
            // The omitted class: the call reports E0224
            // naming the first omitted parameter without a default, after
            // which the arguments are still analyzed (E0201 for `BAD_ARG`).
            Class::Omitted(param) => {
                omitted += 1;
                let id = &case.id;
                let Some((_, diagnostics)) = result else {
                    panic!("{id}: still panics");
                };
                let error_codes: Vec<&str> = diagnostics
                    .iter()
                    .filter(|d| d.is_error())
                    .filter_map(|d| d.code.as_deref())
                    .collect();
                let expected: &[&str] = if case.bad_first {
                    &[codes::E0224, codes::E0201]
                } else {
                    &[codes::E0224]
                };
                assert_eq!(error_codes, expected, "{id}: {diagnostics:?}");
                assert_eq!(
                    diagnostics[0].message,
                    format!(
                        "`f` is missing required argument `{}`",
                        case.param_names[param]
                    ),
                    "{id}"
                );
            }
            // Everything else must be unchanged, byte for byte.
            Class::BindErr | Class::Ok | Class::Count => {
                assert_ne!(outcome, "panic", "{}: panics", case.id);
                unchanged.push_str(&line);
            }
        }
    }
    if let Some(path) = std::env::var_os("PINEWASM_DEF_ARITY_DUMP") {
        std::fs::write(path, &rendered).expect("write the per-program lines");
    }
    assert_eq!(cases.len(), BASELINE_CASES, "sweep size changed");
    assert_eq!(omitted, BASELINE_OMITTED, "omitted class changed");
    assert_eq!(
        fnv1a(&unchanged),
        BASELINE_DIGEST,
        "a program outside the omitted class behaves differently from the \
         expected digest; see the module docs to find it"
    );
}
