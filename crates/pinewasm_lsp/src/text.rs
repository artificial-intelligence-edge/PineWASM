//! UTF-8 compiler span to UTF-16 LSP position conversion.
//!
//! The index itself is transport-free (it also serves the wasm `symbols`
//! export); only the `lsp_types`-typed adapters are `server`-gated.

#[cfg(feature = "server")]
use lsp_types::{Position, Range};
#[cfg(feature = "server")]
use pinewasm_span::Span;

const UTF16_CHECKPOINT_BYTES: usize = 128;

#[derive(Debug, Clone, Copy)]
struct Utf16Checkpoint {
    byte_offset: usize,
    units: u32,
}

/// An immutable line index for one source revision.
#[derive(Debug, Clone)]
pub struct LineIndex {
    text: String,
    line_starts: Vec<usize>,
    /// Byte offsets at the end of each line, before its line terminator.
    line_ends: Vec<usize>,
    /// Sparse whole-document prefix counts; output mapping scans at most one
    /// checkpoint interval instead of rescanning every preceding line.
    utf16_checkpoints: Vec<Utf16Checkpoint>,
}

impl LineIndex {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let mut line_starts = vec![0];
        let mut line_ends = Vec::new();
        let bytes = text.as_bytes();
        let mut offset = 0;
        while offset < bytes.len() {
            match bytes[offset] {
                b'\n' => {
                    line_ends.push(offset);
                    offset += 1;
                    line_starts.push(offset);
                }
                b'\r' => {
                    line_ends.push(offset);
                    offset += if bytes.get(offset + 1) == Some(&b'\n') {
                        2
                    } else {
                        1
                    };
                    line_starts.push(offset);
                }
                _ => offset += 1,
            }
        }
        line_ends.push(text.len());
        let mut utf16_checkpoints = vec![Utf16Checkpoint {
            byte_offset: 0,
            units: 0,
        }];
        let mut next_checkpoint = UTF16_CHECKPOINT_BYTES;
        let mut utf16_units = 0u32;
        for (byte_offset, ch) in text.char_indices() {
            while byte_offset >= next_checkpoint {
                utf16_checkpoints.push(Utf16Checkpoint {
                    byte_offset,
                    units: utf16_units,
                });
                next_checkpoint += UTF16_CHECKPOINT_BYTES;
            }
            utf16_units += ch.len_utf16() as u32;
            let after = byte_offset + ch.len_utf8();
            while after >= next_checkpoint {
                utf16_checkpoints.push(Utf16Checkpoint {
                    byte_offset: after,
                    units: utf16_units,
                });
                next_checkpoint += UTF16_CHECKPOINT_BYTES;
            }
        }
        Self {
            text,
            line_starts,
            line_ends,
            utf16_checkpoints,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Convert a compiler byte offset to a zero-based `(line, UTF-16 column)`
    /// pair. Transport-free twin of [`Self::position`], usable without the
    /// `server` feature (the wasm `symbols` export builds on it).
    pub fn line_utf16(&self, byte_offset: u32) -> (u32, u32) {
        let mut byte = (byte_offset as usize).min(self.text.len());
        while byte > 0 && !self.text.is_char_boundary(byte) {
            byte -= 1;
        }
        let line = match self.line_starts.binary_search(&byte) {
            Ok(index) => index,
            Err(index) => index.saturating_sub(1),
        };
        let byte = byte.min(self.line_ends[line]);
        let character =
            self.utf16_offset(byte as u32) - self.utf16_offset(self.line_starts[line] as u32);
        (line as u32, character)
    }

    /// Convert a UTF-8 byte offset to a zero-based UTF-16 offset from the
    /// beginning of the complete document.
    pub fn utf16_offset(&self, byte_offset: u32) -> u32 {
        let mut byte = (byte_offset as usize).min(self.text.len());
        while byte > 0 && !self.text.is_char_boundary(byte) {
            byte -= 1;
        }
        let checkpoint = self
            .utf16_checkpoints
            .partition_point(|checkpoint| checkpoint.byte_offset <= byte)
            .saturating_sub(1);
        let checkpoint = self.utf16_checkpoints[checkpoint];
        checkpoint.units
            + self.text[checkpoint.byte_offset..byte]
                .encode_utf16()
                .count() as u32
    }

    /// Convert a compiler byte offset to a zero-based UTF-16 LSP position.
    #[cfg(feature = "server")]
    pub fn position(&self, byte_offset: u32) -> Position {
        let (line, character) = self.line_utf16(byte_offset);
        Position::new(line, character)
    }

    /// Convert a zero-based UTF-16 LSP position to a compiler byte offset.
    /// Out-of-range positions clamp to the end of the requested line/file.
    #[cfg(feature = "server")]
    pub fn byte_offset(&self, position: Position) -> u32 {
        self.byte_offset_utf16(position.line, position.character)
    }

    /// Transport-neutral inverse of `line_utf16`, with the same clamping policy.
    pub fn byte_offset_utf16(&self, line: u32, character: u32) -> u32 {
        let line = (line as usize).min(self.line_starts.len().saturating_sub(1));
        let start = self.line_starts[line];
        let end = self.line_ends[line];
        let line_text = &self.text[start..end];
        let target = character as usize;
        let mut utf16 = 0usize;
        for (relative, ch) in line_text.char_indices() {
            if utf16 >= target {
                return (start + relative) as u32;
            }
            let width = ch.len_utf16();
            if utf16 + width > target {
                return (start + relative) as u32;
            }
            utf16 += width;
        }
        end as u32
    }

    #[cfg(feature = "server")]
    pub fn range(&self, span: Span) -> Range {
        Range::new(self.position(span.start), self.position(span.end))
    }

    #[cfg(feature = "server")]
    pub fn line_end(&self, line: u32) -> Position {
        let index = (line as usize).min(self.line_starts.len().saturating_sub(1));
        let end = self.line_ends[index];
        self.position(end as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "server")]
    use pinewasm_span::SourceId;

    #[test]
    fn converts_utf8_bytes_to_transport_free_utf16_columns() {
        let index = LineIndex::new("a😀b\nç");
        assert_eq!(index.line_utf16(0), (0, 0));
        assert_eq!(index.line_utf16(1), (0, 1));
        assert_eq!(index.line_utf16(5), (0, 3));
        assert_eq!(index.line_utf16(6), (0, 4));
        assert_eq!(index.line_utf16(7), (1, 0));
        assert_eq!(index.line_utf16(9), (1, 1));
    }

    #[test]
    fn handles_lf_crlf_and_cr_with_utf16_columns() {
        for (newline, next_line_start) in [("\n", 6), ("\r\n", 7), ("\r", 6)] {
            let text = format!("😀x{newline}y");
            let index = LineIndex::new(text);

            assert_eq!(index.line_count(), 2, "newline {newline:?}");
            assert_eq!(index.line_utf16(4), (0, 2), "newline {newline:?}");
            assert_eq!(index.line_utf16(5), (0, 3), "newline {newline:?}");
            for byte_offset in 5..next_line_start {
                assert_eq!(
                    index.line_utf16(byte_offset),
                    (0, 3),
                    "newline {newline:?}, byte {byte_offset}"
                );
            }
            assert_eq!(
                index.line_utf16(next_line_start),
                (1, 0),
                "newline {newline:?}"
            );
            assert_eq!(index.byte_offset_utf16(0, 2), 4, "newline {newline:?}");
            assert_eq!(index.byte_offset_utf16(0, 3), 5, "newline {newline:?}");
            assert_eq!(
                index.byte_offset_utf16(1, 0),
                next_line_start,
                "newline {newline:?}"
            );
            assert_eq!(index.byte_offset_utf16(0, 4), 5, "newline {newline:?}");
        }
    }

    #[test]
    fn sparse_utf16_checkpoints_preserve_long_line_endings_and_non_bmp_offsets() {
        let text = format!(
            "{}😀\r{}😀\n{}",
            "a".repeat(126),
            "b".repeat(127),
            "c".repeat(132)
        );
        let index = LineIndex::new(text.clone());
        for (byte, _) in text
            .char_indices()
            .chain(std::iter::once((text.len(), '\0')))
        {
            assert_eq!(
                index.utf16_offset(byte as u32),
                text[..byte].encode_utf16().count() as u32,
                "byte offset {byte}"
            );
        }
        let cr = text.find('\r').unwrap() as u32;
        assert_eq!(index.line_utf16(cr), (0, 128));
        assert_eq!(index.line_utf16(cr + 1), (1, 0));
        let lf = text.find('\n').unwrap() as u32;
        assert_eq!(index.line_utf16(lf), (1, 129));
        assert_eq!(index.line_utf16(lf + 1), (2, 0));
    }

    #[cfg(feature = "server")]
    #[test]
    fn converts_utf8_bytes_to_lsp_positions() {
        let index = LineIndex::new("a😀b\nç");
        assert_eq!(index.position(0), Position::new(0, 0));
        assert_eq!(index.position(1), Position::new(0, 1));
        assert_eq!(index.position(5), Position::new(0, 3));
        assert_eq!(index.position(6), Position::new(0, 4));
        assert_eq!(index.position(7), Position::new(1, 0));
        assert_eq!(index.position(9), Position::new(1, 1));
    }

    #[cfg(feature = "server")]
    #[test]
    fn round_trips_positions_and_ranges() {
        let index = LineIndex::new("a😀b\nç");
        assert_eq!(index.byte_offset(Position::new(0, 3)), 5);
        assert_eq!(index.byte_offset(Position::new(1, 1)), 9);
        assert_eq!(
            index.range(Span::new(SourceId::MAIN, 1, 5)),
            Range::new(Position::new(0, 1), Position::new(0, 3))
        );
    }
}
