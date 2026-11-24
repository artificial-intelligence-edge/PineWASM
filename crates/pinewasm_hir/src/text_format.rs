//! Pine's nominal text formatting domain. These are our canonical wire
//! strings used by PineWASM.

pub const TEXT_FORMAT_TYPE: &str = "text_format";
pub const TEXT_FORMAT_NONE: &str = "text.format_none";
pub const TEXT_FORMAT_BOLD: &str = "text.format_bold";
pub const TEXT_FORMAT_ITALIC: &str = "text.format_italic";
pub const TEXT_FORMAT_BOTH: &str = "text.format_bold+text.format_italic";
pub const TEXT_FORMAT_VALUES: &[&str] = &[
    TEXT_FORMAT_NONE,
    TEXT_FORMAT_BOLD,
    TEXT_FORMAT_ITALIC,
    TEXT_FORMAT_BOTH,
];

fn flags(value: &str) -> Option<(bool, bool)> {
    Some(match value {
        TEXT_FORMAT_NONE => (false, false),
        TEXT_FORMAT_BOLD => (true, false),
        TEXT_FORMAT_ITALIC => (false, true),
        TEXT_FORMAT_BOTH => (true, true),
        _ => return None,
    })
}

/// Idempotent, commutative union over the four supported formatting states.
pub fn text_format_union(lhs: &str, rhs: &str) -> Option<&'static str> {
    let (lb, li) = flags(lhs)?;
    let (rb, ri) = flags(rhs)?;
    Some(match (lb || rb, li || ri) {
        (false, false) => TEXT_FORMAT_NONE,
        (true, false) => TEXT_FORMAT_BOLD,
        (false, true) => TEXT_FORMAT_ITALIC,
        (true, true) => TEXT_FORMAT_BOTH,
    })
}
