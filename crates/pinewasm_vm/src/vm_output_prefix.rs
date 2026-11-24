//! Borrowed visible-prefix projection used by the actual outcome constructor.

/// Select exactly the caller's visibility bound; oversized bounds retain the
/// original slice panic. The outcome constructor clamps to VM bars before this
/// call and retains its existing `to_vec` cloning and allocation afterward.
pub(crate) fn visible_prefix<T>(row: &[T], visible: usize) -> &[T] {
    &row[..visible]
}

#[cfg(test)]
mod tests {
    use super::visible_prefix;

    #[test]
    fn prefixes_are_borrowed_exactly_and_preserve_values() {
        let row = [
            String::from("first"),
            String::from("second"),
            String::from("failed"),
        ];
        for visible in 0..=row.len() {
            let prefix = visible_prefix(&row, visible);
            assert_eq!(prefix, &row[..visible]);
            assert_eq!(prefix.as_ptr(), row.as_ptr());
        }
        assert_eq!(visible_prefix(&row, 2).to_vec(), ["first", "second"]);
        assert_eq!(visible_prefix(&row, 3)[2], "failed");
        assert!(visible_prefix::<String>(&[], 0).is_empty());
    }

    #[test]
    #[should_panic(expected = "range end index 3 out of range for slice of length 2")]
    fn oversized_visibility_panics_instead_of_saturating() {
        visible_prefix(&[1, 2], 3);
    }
}
