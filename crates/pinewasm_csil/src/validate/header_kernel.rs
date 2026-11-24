//! Formatting-free header and metadata predicates used by production validation.
//!
//! Diagnostic construction and ordering remain in `validate`. Metadata is
//! deliberately a separate kernel: `NoFunctions` returns before checking it.
use crate::{
    CSIL_VERSION, LANGUAGE_VERSION, MAX_BARS_BACK_LIMIT, RUNTIME_ABI_VERSION, TA_INPUT_ABI_VERSION,
};

pub(crate) struct HeaderFacts<'a> {
    pub magic: &'a [u8],
    pub csil_version: u32,
    pub language_version: u32,
    pub runtime_abi_version: u32,
}

pub(crate) struct HeaderIssues {
    pub magic: bool,
    pub csil_version: bool,
    pub language_version: bool,
    pub runtime_abi_version: bool,
}

fn bytes_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut i = 0;
    while i < left.len() {
        if left[i] != right[i] {
            return false;
        }
        i += 1;
    }
    true
}

// The caller supplies the actual format constant's bytes; this avoids modeling
// Rust string construction in the extracted predicate. Production passes
// `CSIL_MAGIC.as_bytes()`, keeping that projection obligation explicit.
pub(crate) fn check_header(facts: HeaderFacts<'_>, expected_magic: &[u8]) -> HeaderIssues {
    HeaderIssues {
        magic: !bytes_equal(facts.magic, expected_magic),
        csil_version: facts.csil_version != CSIL_VERSION,
        language_version: facts.language_version != LANGUAGE_VERSION,
        runtime_abi_version: facts.runtime_abi_version != RUNTIME_ABI_VERSION
            && facts.runtime_abi_version != TA_INPUT_ABI_VERSION
            && facts.runtime_abi_version != crate::INDEXED_SCAN_ABI_VERSION,
    }
}

#[allow(
    clippy::manual_range_contains,
    reason = "Keep primitive comparisons explicit for the extracted production admission kernel"
)]
pub(crate) fn max_bars_back_admitted(value: u32) -> bool {
    1 <= value && value <= MAX_BARS_BACK_LIMIT
}

#[cfg(test)]
mod tests {
    use super::super::{validate, ValidationError};
    use super::*;
    use crate::{CsilHeader, CsilProgram, CSIL_MAGIC};

    fn facts(h: &CsilHeader) -> HeaderFacts<'_> {
        HeaderFacts {
            magic: h.magic.as_bytes(),
            csil_version: h.csil_version,
            language_version: h.language_version,
            runtime_abi_version: h.runtime_abi_version,
        }
    }

    fn program() -> CsilProgram {
        let parsed = pinewasm_parse::parse("//@version=6\nindicator(\"Header\")\nplot(close)\n");
        assert!(!pinewasm_span::has_errors(&parsed.diagnostics));
        let analyzed = pinewasm_semantics::analyze(&parsed.program);
        assert!(!pinewasm_span::has_errors(&analyzed.diagnostics));
        crate::lower_mir(&pinewasm_mir::lower(&analyzed.hir)).expect("lowers")
    }

    #[test]
    fn each_header_field_and_both_abis() {
        let mut p = program();
        for abi in [
            RUNTIME_ABI_VERSION,
            TA_INPUT_ABI_VERSION,
            crate::INDEXED_SCAN_ABI_VERSION,
        ] {
            p.header.runtime_abi_version = abi;
            let issues = check_header(facts(&p.header), CSIL_MAGIC.as_bytes());
            assert!(
                !issues.magic
                    && !issues.csil_version
                    && !issues.language_version
                    && !issues.runtime_abi_version
            );
            assert_eq!(validate(&p), Ok(()));
        }
        for field in 0..4 {
            p.header = CsilHeader::default();
            match field {
                0 => p.header.magic = "CSIL\0".into(),
                1 => p.header.csil_version = u32::MAX,
                2 => p.header.language_version = u32::MAX,
                _ => p.header.runtime_abi_version = u32::MAX,
            }
            let i = check_header(facts(&p.header), CSIL_MAGIC.as_bytes());
            assert_eq!(
                [
                    i.magic,
                    i.csil_version,
                    i.language_version,
                    i.runtime_abi_version
                ],
                [field == 0, field == 1, field == 2, field == 3]
            );
            let errors = validate(&p).unwrap_err();
            assert_eq!(errors.len(), 1);
            assert!(matches!(errors[0], ValidationError::Header(_)));
        }
    }

    #[test]
    fn magic_is_exact_utf8_bytes() {
        let mut h = CsilHeader::default();
        for bad in ["", "CSI", "CSIL!", "CSIl", "ＣSIL", "CSIL\0"] {
            h.magic = bad.into();
            assert!(
                check_header(facts(&h), CSIL_MAGIC.as_bytes()).magic,
                "{bad:?}"
            );
        }
        h.magic = CSIL_MAGIC.into();
        assert!(!check_header(facts(&h), CSIL_MAGIC.as_bytes()).magic);
        // The predicate uses its explicit expected bytes, not a hidden constant.
        assert!(check_header(facts(&h), b"OTHER").magic);
        h.magic = "OTHER".into();
        assert!(!check_header(facts(&h), b"OTHER").magic);
    }

    #[test]
    fn empty_functions_preserves_header_order_and_skips_metadata() {
        let mut p = program();
        p.functions.clear();
        p.metadata.max_bars_back = 0;
        p.header.magic = "bad".into();
        p.header.csil_version = 99;
        p.header.language_version = 99;
        p.header.runtime_abi_version = 99;
        assert_eq!(validate(&p).unwrap_err(), vec![
            ValidationError::Header(format!("magic is {:?}, expected {CSIL_MAGIC:?}", p.header.magic)),
            ValidationError::Header(format!("CSIL version 99 is not supported (expected {CSIL_VERSION})")),
            ValidationError::Header(format!("source-language version 99 is not supported (expected {LANGUAGE_VERSION})")),
            ValidationError::Header(format!("runtime ABI version 99 is not supported (expected {RUNTIME_ABI_VERSION}, {TA_INPUT_ABI_VERSION}, or {})", crate::INDEXED_SCAN_ABI_VERSION)),
            ValidationError::NoFunctions,
        ]);
        p.header = CsilHeader::default();
        for abi in [
            RUNTIME_ABI_VERSION,
            TA_INPUT_ABI_VERSION,
            crate::INDEXED_SCAN_ABI_VERSION,
        ] {
            p.header.runtime_abi_version = abi;
            assert_eq!(validate(&p), Err(vec![ValidationError::NoFunctions]));
        }
    }

    #[test]
    fn metadata_boundaries_and_diagnostic() {
        let mut p = program();
        for n in [1, MAX_BARS_BACK_LIMIT] {
            p.metadata.max_bars_back = n;
            assert!(max_bars_back_admitted(n));
            assert_eq!(validate(&p), Ok(()));
        }
        for n in [0, MAX_BARS_BACK_LIMIT + 1, u32::MAX] {
            p.metadata.max_bars_back = n;
            assert!(!max_bars_back_admitted(n));
            assert_eq!(
                validate(&p),
                Err(vec![ValidationError::Metadata(format!(
                    "max_bars_back {n} is outside 1..={MAX_BARS_BACK_LIMIT} (default {})",
                    crate::MAX_BARS_BACK_DEFAULT
                ))])
            );
        }
    }
}
