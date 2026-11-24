//! Immediate input diagnostics: owned decisions, borrowed payloads from the program.
use super::input_domain_reason::InputDomainReason;
use pinewasm_hir::{
    InputKind, MAX_INPUT_ACTIVE_EXPRESSION_DEPTH, MAX_INPUT_ACTIVE_EXPRESSION_NODES,
    MAX_INPUT_ACTIVE_STRING_BYTES,
};

pub(super) enum ValueLocation {
    Default,
    Minimum,
    Maximum,
    Step,
    Option,
}

pub(super) enum Request<'a> {
    IdMismatch {
        declared: u32,
    },
    DefaultKindMismatch {
        kind: InputKind,
    },
    Domain {
        location: ValueLocation,
        reason: InputDomainReason,
    },
    ConstraintKindMismatch {
        location: &'a ValueLocation,
    },
    OptionKindMismatch,
    NumericConstraintsNotAllowed {
        kind: InputKind,
    },
    OptionsNotAllowed {
        kind: InputKind,
    },
    BoundsOptionsExclusive,
    DefaultBelowMinimum,
    DefaultAboveMaximum,
    DefaultAbsentFromOptions,
    EnumNameMismatch {
        name: &'a String,
        enum_name: &'a String,
    },
    DuplicateEnumIndex {
        index: u32,
    },
    DuplicateEnumMember {
        member: &'a String,
    },
    EnumDefaultAbsent,
    DependencyNotPrior {
        dependency: u32,
    },
    DependenciesNotSorted,
    DirectNotPrior {
        input: u32,
    },
    DirectNotBool {
        input: u32,
    },
    DirectMissing {
        input: u32,
    },
    ExpressionDependenciesMismatch {
        actual: &'a Vec<u32>,
        declared: &'a Vec<u32>,
    },
    ExpressionNodeLimit,
    ExpressionDepthLimit,
    ExpressionSeriesSource,
    ExpressionNonfiniteFloat,
    ExpressionColorTransparency {
        t: f64,
    },
    ExpressionStringLimit,
    ExpressionInputNotPrior {
        input: u32,
    },
    InvalidCallName,
    CallArity {
        function: &'a String,
        min: usize,
        max: usize,
        actual: usize,
    },
    UnsupportedCall {
        function: &'a String,
    },
    CallArgumentLimit {
        function: &'a String,
    },
}

// &self does not assert purity or termination. A generic renderer can panic or
// use interior state; the production message text comes from ProductionRenderer.
pub(super) trait Renderer {
    fn render(&self, input_index: usize, request: Request<'_>) -> String;
}

pub(super) struct ProductionRenderer;

fn location_label(location: &ValueLocation) -> &'static str {
    match location {
        ValueLocation::Default => "defval",
        ValueLocation::Minimum => "minval",
        ValueLocation::Maximum => "maxval",
        ValueLocation::Step => "step",
        ValueLocation::Option => "an options entry",
    }
}

impl Renderer for ProductionRenderer {
    fn render(&self, input_index: usize, request: Request<'_>) -> String {
        match request {
            Request::IdMismatch { declared } =>
                format!("id {declared} does not match its index {input_index}"),
            Request::DefaultKindMismatch { kind } =>
                format!("defval does not match declared kind `input.{}`", kind.describe()),
            Request::Domain { location, reason } => {
                let message = reason.message();
                // Keep the literal prefixes as written: changing them into a
                // dynamic label would also change format!'s capacity hint.
                match location {
                    ValueLocation::Default => format!("defval {message}"),
                    ValueLocation::Option => format!("an options entry {message}"),
                    other => {
                        let label = location_label(&other);
                        format!("{label} {message}")
                    }
                }
            }
            Request::ConstraintKindMismatch { location } => {
                let label = location_label(location);
                format!("{label} does not match declared kind")
            }
            Request::OptionKindMismatch =>
                "an options entry does not match declared kind".to_string(),
            Request::NumericConstraintsNotAllowed { kind } =>
                format!("`input.{}` does not take numeric constraints", kind.describe()),
            Request::OptionsNotAllowed { kind } =>
                format!("`input.{}` does not take an options list", kind.describe()),
            Request::BoundsOptionsExclusive =>
                "numeric bounds/step and options are mutually exclusive".to_string(),
            Request::DefaultBelowMinimum => "defval is below minval".to_string(),
            Request::DefaultAboveMaximum => "defval is above maxval".to_string(),
            Request::DefaultAbsentFromOptions =>
                "options do not contain the declared default".to_string(),
            Request::EnumNameMismatch { name, enum_name } =>
                format!("enum option `{name}` does not match declaring enum `{enum_name}`"),
            Request::DuplicateEnumIndex { index } => format!("duplicate enum option index {index}"),
            Request::DuplicateEnumMember { member } =>
                format!("duplicate enum option member `{member}`"),
            Request::EnumDefaultAbsent =>
                "enum options do not contain the declared default member".to_string(),
            Request::DependencyNotPrior { dependency } =>
                format!("active dependency {dependency} must reference a prior input"),
            Request::DependenciesNotSorted => "active dependencies must be sorted and unique".to_string(),
            Request::DirectNotPrior { input } =>
                format!("direct active dependency {input} must reference a prior input"),
            Request::DirectNotBool { input } =>
                format!("direct active dependency {input} must reference a bool input"),
            Request::DirectMissing { input } =>
                format!("direct active dependency {input} is missing from dependencies"),
            Request::ExpressionDependenciesMismatch { actual, declared } =>
                format!("active expression dependencies {actual:?} do not match declared dependencies {declared:?}"),
            Request::ExpressionNodeLimit =>
                format!("active expression exceeds the {MAX_INPUT_ACTIVE_EXPRESSION_NODES}-node limit"),
            Request::ExpressionDepthLimit =>
                format!("active expression exceeds the {MAX_INPUT_ACTIVE_EXPRESSION_DEPTH}-level nesting limit"),
            Request::ExpressionSeriesSource => "active expression cannot contain a series source value".to_string(),
            Request::ExpressionNonfiniteFloat => "active expression float value must be finite".to_string(),
            Request::ExpressionColorTransparency { t } =>
                format!("active expression color transparency {t} out of range (0..=100)"),
            Request::ExpressionStringLimit =>
                format!("active expression string exceeds the {MAX_INPUT_ACTIVE_STRING_BYTES}-byte limit"),
            Request::ExpressionInputNotPrior { input } =>
                format!("active expression input {input} must reference a prior input"),
            Request::InvalidCallName => "active expression call has an invalid function name".to_string(),
            Request::CallArity { function, min, max, actual } =>
                format!("active expression call `{function}` expects {min}..={max} arguments, got {actual}"),
            Request::UnsupportedCall { function } =>
                format!("active expression call `{function}` is not supported by bounded hosts"),
            Request::CallArgumentLimit { function } =>
                format!("active expression call `{function}` has too many arguments"),
        }
    }
}

// Keep borrowed requests and renderer outside the mutable diagnostic-vector call.
// This is an immediate production boundary, not a deferred request queue.
pub(super) fn render<R: Renderer>(renderer: &R, index: usize, request: Request<'_>) -> String {
    renderer.render(index, request)
}
