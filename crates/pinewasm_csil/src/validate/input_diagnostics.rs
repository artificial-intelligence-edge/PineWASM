//! Immediate input diagnostic append into the caller's error vector.
use super::ValidationError;

pub(super) fn emit(errors: &mut Vec<ValidationError>, index: usize, message: String) {
    errors.push(ValidationError::Input { index, message });
}
