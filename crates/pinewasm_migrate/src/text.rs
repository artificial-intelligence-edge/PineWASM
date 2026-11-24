//! Text splicing for the migration passes.
//!
//! Every pass edits the *current* source text rather than printing a new one
//! from the AST. The parser discards comments and most layout, so reprinting
//! would silently destroy them; splicing only the ranges a rule changes keeps
//! everything else byte-for-byte. An [`EditSet`] holds one iteration's edits,
//! refuses any edit that would overlap another (the rule is simply retried on
//! the next iteration, against re-parsed text), and applies them in one step.
//! [`OffsetMap`] remembers where each iteration moved text so findings can be
//! reported against the caller's original source.

/// Where an insertion sits relative to other insertions at the same offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum InsertClass {
    /// Whole lines hoisted in front of a statement. These come first so a
    /// statement's own wrapping never lands above its hoisted temporaries.
    Line,
    /// The closing half of a wrap (`)`), innermost first.
    Close,
    /// The opening half of a wrap (`bool(`), outermost first.
    Open,
    /// Text appended after an argument list entry or similar point.
    Point,
}

#[derive(Debug, Clone)]
struct Insert {
    pos: usize,
    text: String,
    class: InsertClass,
    /// Width of the wrapped node, used to nest wraps that share an offset.
    width: usize,
    seq: usize,
}

#[derive(Debug, Clone)]
struct Replace {
    start: usize,
    end: usize,
    text: String,
}

/// One region of an applied iteration: `old` bytes of the previous text became
/// `new` bytes of the next text.
#[derive(Debug, Clone, Copy)]
struct Moved {
    old_start: usize,
    old_end: usize,
    new_start: usize,
    new_end: usize,
}

/// The edits one iteration of a pass wants to make.
#[derive(Debug, Default)]
pub(crate) struct EditSet {
    replaces: Vec<Replace>,
    inserts: Vec<Insert>,
    seq: usize,
}

impl EditSet {
    pub(crate) fn is_empty(&self) -> bool {
        self.replaces.is_empty() && self.inserts.is_empty()
    }

    /// Whether any edit already touches the interior of `[start, end)` or
    /// replaces a range overlapping it. Rules that need to move or rewrite a
    /// whole range check this first and defer to the next iteration.
    pub(crate) fn touches(&self, start: usize, end: usize) -> bool {
        self.replaces.iter().any(|r| {
            r.start < end && start < r.end
                || (r.start == r.end && r.start >= start && r.start <= end)
        }) || self.inserts.iter().any(|i| match i.class {
            // A wrap opening at `start` or closing at `end` belongs to a
            // node inside (or equal to) the range, so it moves with it.
            InsertClass::Open => i.pos >= start && i.pos < end,
            InsertClass::Close => i.pos > start && i.pos <= end,
            InsertClass::Line | InsertClass::Point => i.pos > start && i.pos < end,
        })
    }

    fn replace_conflicts(&self, start: usize, end: usize) -> bool {
        self.replaces.iter().any(|r| {
            if start == end || r.start == r.end {
                // Pure insertions expressed as empty replaces never overlap,
                // but two at the same point would be ambiguous.
                r.start == start && r.end == end || (start < r.end && r.start < end)
            } else {
                start < r.end && r.start < end
            }
        }) || self.inserts.iter().any(|i| i.pos > start && i.pos < end)
    }

    /// Replace `[start, end)` with `text`. Returns `false` (and records
    /// nothing) when the range collides with an edit already in the set.
    pub(crate) fn replace(&mut self, start: usize, end: usize, text: impl Into<String>) -> bool {
        if self.replace_conflicts(start, end) {
            return false;
        }
        self.replaces.push(Replace {
            start,
            end,
            text: text.into(),
        });
        true
    }

    fn insert_inside_replace(&self, pos: usize) -> bool {
        self.replaces.iter().any(|r| pos > r.start && pos < r.end)
    }

    fn push_insert(&mut self, pos: usize, text: String, class: InsertClass, width: usize) -> bool {
        if self.insert_inside_replace(pos) {
            return false;
        }
        self.seq += 1;
        self.inserts.push(Insert {
            pos,
            text,
            class,
            width,
            seq: self.seq,
        });
        true
    }

    /// Surround `[start, end)` with `open` and `close`, keeping any edits
    /// already made inside the range. Wrapping the same range twice in one
    /// iteration is refused so nesting order is never ambiguous.
    pub(crate) fn wrap(&mut self, start: usize, end: usize, open: &str, close: &str) -> bool {
        let width = end - start;
        let same = self.inserts.iter().any(|i| {
            (i.class == InsertClass::Open && i.pos == start && i.width == width)
                || (i.class == InsertClass::Close && i.pos == end && i.width == width)
        });
        if same || self.insert_inside_replace(start) || self.insert_inside_replace(end) {
            return false;
        }
        self.push_insert(start, open.to_string(), InsertClass::Open, width)
            && self.push_insert(end, close.to_string(), InsertClass::Close, width)
    }

    /// Insert whole lines in front of the line that starts at `line_start`.
    pub(crate) fn insert_lines(&mut self, line_start: usize, text: impl Into<String>) -> bool {
        self.push_insert(line_start, text.into(), InsertClass::Line, 0)
    }

    /// Insert `text` at `pos` (after any wraps closing there).
    pub(crate) fn insert_at(&mut self, pos: usize, text: impl Into<String>) -> bool {
        self.push_insert(pos, text.into(), InsertClass::Point, 0)
    }

    /// Apply every edit to `source`, returning the new text and the map from
    /// new offsets back to `source` offsets.
    pub(crate) fn apply(mut self, source: &str) -> (String, IterationMap) {
        enum Piece {
            Insert(Insert),
            Replace(Replace),
        }
        let mut pieces: Vec<(usize, u8, Piece)> = Vec::new();
        for insert in self.inserts.drain(..) {
            pieces.push((insert.pos, 0, Piece::Insert(insert)));
        }
        for replace in self.replaces.drain(..) {
            pieces.push((replace.start, 1, Piece::Replace(replace)));
        }
        pieces.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.cmp(&b.1))
                .then_with(|| match (&a.2, &b.2) {
                    (Piece::Insert(x), Piece::Insert(y)) => x
                        .class
                        .cmp(&y.class)
                        .then_with(|| match x.class {
                            InsertClass::Close => x.width.cmp(&y.width),
                            InsertClass::Open => y.width.cmp(&x.width),
                            _ => std::cmp::Ordering::Equal,
                        })
                        .then(x.seq.cmp(&y.seq)),
                    _ => std::cmp::Ordering::Equal,
                })
        });
        let mut out = String::with_capacity(source.len() + 256);
        let mut moved = Vec::new();
        let mut cursor = 0usize;
        for (_, _, piece) in pieces {
            match piece {
                Piece::Insert(insert) => {
                    if insert.pos > cursor {
                        out.push_str(&source[cursor..insert.pos]);
                        cursor = insert.pos;
                    }
                    let new_start = out.len();
                    out.push_str(&insert.text);
                    moved.push(Moved {
                        old_start: insert.pos,
                        old_end: insert.pos,
                        new_start,
                        new_end: out.len(),
                    });
                }
                Piece::Replace(replace) => {
                    if replace.start > cursor {
                        out.push_str(&source[cursor..replace.start]);
                    }
                    let new_start = out.len();
                    // Keep tokens apart: `f(x)and y` must not become `tand y`.
                    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
                    if replace.text.starts_with(word) && out.ends_with(word) {
                        out.push(' ');
                    }
                    out.push_str(&replace.text);
                    if replace.text.ends_with(word)
                        && source[replace.end.min(source.len())..].starts_with(word)
                    {
                        out.push(' ');
                    }
                    moved.push(Moved {
                        old_start: replace.start,
                        old_end: replace.end,
                        new_start,
                        new_end: out.len(),
                    });
                    cursor = replace.end.max(cursor);
                }
            }
        }
        out.push_str(&source[cursor.min(source.len())..]);
        (out, IterationMap { moved })
    }
}

/// How one applied iteration moved text.
#[derive(Debug, Clone, Default)]
pub(crate) struct IterationMap {
    moved: Vec<Moved>,
}

impl IterationMap {
    /// Map an offset in the iteration's output back to its input. Offsets
    /// inside inserted or replaced text map to the start of the region they
    /// replaced.
    fn map_back(&self, pos: usize) -> usize {
        let mut delta: isize = 0;
        for m in &self.moved {
            if pos < m.new_start {
                break;
            }
            if pos < m.new_end {
                return m.old_start;
            }
            delta = m.old_end as isize - m.new_end as isize;
        }
        (pos as isize + delta).max(0) as usize
    }
}

/// The composition of every applied iteration, newest last.
#[derive(Debug, Clone, Default)]
pub(crate) struct OffsetMap {
    iterations: Vec<IterationMap>,
}

impl OffsetMap {
    pub(crate) fn push(&mut self, map: IterationMap) {
        self.iterations.push(map);
    }

    /// Map an offset in the current text back to the original source.
    pub(crate) fn to_original(&self, pos: usize) -> usize {
        self.iterations
            .iter()
            .rev()
            .fold(pos, |pos, iteration| iteration.map_back(pos))
    }
}

/// Byte offset of the start of the line containing `pos`.
pub(crate) fn line_start(source: &str, pos: usize) -> usize {
    source[..pos.min(source.len())]
        .rfind('\n')
        .map_or(0, |i| i + 1)
}

/// Byte offset just past the end of the line containing `pos` (the newline
/// itself is excluded).
pub(crate) fn line_end(source: &str, pos: usize) -> usize {
    source[pos.min(source.len())..]
        .find('\n')
        .map_or(source.len(), |i| pos + i)
}

/// The leading whitespace of the line containing `pos`.
pub(crate) fn indent_at(source: &str, pos: usize) -> &str {
    let start = line_start(source, pos);
    let rest = &source[start..];
    let width = rest.len() - rest.trim_start_matches([' ', '\t']).len();
    &rest[..width]
}

/// The block indentation unit the script uses: a tab when the first indented
/// block line starts with one, otherwise four spaces (Pine's block unit).
pub(crate) fn indent_unit(source: &str) -> &'static str {
    for line in source.lines() {
        let trimmed = line.trim_start_matches([' ', '\t']);
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.len() == line.len() {
            continue;
        }
        return if line.starts_with('\t') { "\t" } else { "    " };
    }
    "    "
}

/// The single smallest edit turning `old` into `new`: the common prefix and
/// suffix are trimmed at line boundaries so the edit covers whole lines.
pub(crate) fn line_diff(old: &str, new: &str) -> Option<(usize, usize, String)> {
    if old == new {
        return None;
    }
    let prefix = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    // Back up to the start of the line (always a char boundary).
    let prefix = old.as_bytes()[..prefix]
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |i| i + 1);
    let max_suffix = (old.len() - prefix).min(new.len() - prefix);
    let mut suffix = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    // Shrink the suffix until the replaced region ends just after a newline,
    // so the edit covers complete lines (and ends on a char boundary).
    while suffix > 0 && (suffix == old.len() || old.as_bytes()[old.len() - suffix - 1] != b'\n') {
        suffix -= 1;
    }
    Some((
        prefix,
        old.len() - suffix,
        new[prefix..new.len() - suffix].to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_nest_and_replaces_splice() {
        let src = "x = a + b";
        let mut edits = EditSet::default();
        assert!(edits.wrap(4, 9, "bool(", ")"));
        assert!(edits.wrap(4, 5, "int(", ")"));
        assert!(edits.replace(8, 9, "c"));
        assert!(!edits.replace(7, 9, "z"), "overlapping replace is refused");
        assert!(edits.insert_lines(0, "t = 1\n"));
        let (out, map) = edits.apply(src);
        assert_eq!(out, "t = 1\nx = bool(int(a) + c)");
        let mut offsets = OffsetMap::default();
        offsets.push(map);
        assert_eq!(offsets.to_original(out.find('x').unwrap()), 0);
        assert_eq!(offsets.to_original(out.find('c').unwrap()), 8);
    }

    #[test]
    fn line_diff_covers_whole_lines() {
        let (start, end, text) = line_diff("a\nb\nc\n", "a\nB\nc\n").unwrap();
        assert_eq!((start, end, text.as_str()), (2, 4, "B\n"));
    }
}
