//! Structured production function diagnostics. Only `VecSink` renders strings.
use super::{
    function_structure::StructureIssue, height_error, instruction_offset, typed_error, HeightErr,
    TypedErr, ValidationError, MAX_DECLARED_STACK,
};

#[derive(Debug, Clone)]
pub(super) enum Issue {
    EmptyBody,
    SourceMapLength {
        actual: usize,
        expected: usize,
    },
    Structure {
        offset: usize,
        issue: StructureIssue,
    },
    DeclaredStackLimit {
        declared: u32,
    },
    Preflight {
        error: HeightErr,
    },
    Htf {
        offset: usize,
        message: String,
    },
    Typed {
        error: TypedErr,
    },
    Underdeclared {
        declared: u32,
        seen: u32,
    },
}
#[cfg(test)]
pub(super) trait Sink {
    fn emit(&mut self, issue: Issue);
}
/// Immutable diagnostic context with an explicit mutable accumulator.
pub(super) trait Emitter<S> {
    fn emit(&self, state: &mut S, issue: Issue);
}
#[cfg(test)]
pub(super) struct SinkEmitter;
#[cfg(test)]
impl<S: Sink> Emitter<S> for SinkEmitter {
    fn emit(&self, state: &mut S, issue: Issue) {
        Sink::emit(state, issue);
    }
}
#[cfg(test)]
pub(super) struct VecSink<'a> {
    pub name: &'a str,
    pub state_count: usize,
    pub errors: &'a mut Vec<ValidationError>,
}
#[cfg(test)]
impl Sink for VecSink<'_> {
    fn emit(&mut self, issue: Issue) {
        self.errors.push(render(self.name, self.state_count, issue));
    }
}
#[cfg(test)]
impl<S: Sink> super::function_structure::StructureIssueSink for S {
    fn emit(&mut self, offset: usize, issue: StructureIssue) {
        Sink::emit(self, Issue::Structure { offset, issue });
    }
}
pub(super) fn render(name: &str, state_count: usize, issue: Issue) -> ValidationError {
    match issue {
        Issue::EmptyBody => ValidationError::Structure {
            function: name.to_owned(),
            message: "function body is empty".to_string(),
        },
        Issue::SourceMapLength { actual, expected } => ValidationError::Structure {
            function: name.to_owned(),
            message: format!("source map has {actual} entries for {expected} instructions"),
        },
        Issue::Structure { offset, issue } => ValidationError::AtInstruction {
            function: name.to_owned(),
            offset: instruction_offset(offset),
            message: issue.message(),
        },
        Issue::DeclaredStackLimit { declared } => ValidationError::Structure {
            function: name.to_owned(),
            message: format!(
                "declared max_stack {declared} exceeds the limit of {MAX_DECLARED_STACK}"
            ),
        },
        Issue::Preflight { error } => height_error(name, error, state_count),
        Issue::Htf { offset, message } => ValidationError::AtInstruction {
            function: name.to_owned(),
            offset: instruction_offset(offset),
            message,
        },
        Issue::Typed { error } => typed_error(name, error, state_count),
        Issue::Underdeclared { declared, seen } => ValidationError::Structure {
            function: name.to_owned(),
            message: format!("declared max_stack {declared} understates actual usage {seen}"),
        },
    }
}

#[cfg(test)]
mod tests;
