//! The builtin tables the compiler consults, as one JSON document.
//!
//! The builtin surface is described by several tables: the semantic registry,
//! the TA engine signatures, the collection call surfaces, the drawing-call
//! surface, the builtin series, the namespaced constants, the type casts, the
//! `footprint`/`volume_row` accessors and the `strategy()` declaration
//! parameters. A few calls are analyzed by bespoke code with no parameter
//! table (`request.footprint`, `library()`, `footprint.get_row_by_price`) and
//! are not listed. The catalog reports what each table declares under that
//! table's own key, so a disagreement between two tables stays visible rather
//! than being reconciled here. It does not describe how an analyzer treats any
//! particular call; that is determined by the analyzer code.

use pinewasm_hir::drawing::{
    drawing_function_surface, drawing_series_surface, DrawingParameterSpec, DrawingReturnQualifier,
};
use pinewasm_hir::footprint::{FpField, FOOTPRINT_FIELDS, VOLUME_ROW_FIELDS};
use pinewasm_hir::{BuiltinSeries, Qualifier, STRATEGY_DECL_PARAMS};
use pinewasm_semantics::{
    cast_result_type, generic_input_source_params, BuiltinConstant, BuiltinFunction,
    BuiltinFunctionKind, BuiltinRegistry, ParamSpec, TaParam, TaParamKind, CAST_FUNCTION_NAMES,
    TIMESTAMP_DATE_STRING_PARAM_NAMES, TIMESTAMP_TZ_PARAM_NAMES, TIMESTAMP_UTC_PARAM_NAMES,
};
use serde_json::{Map, Value};

/// Version of the catalog layout. A change that removes a field or changes
/// what a field means increments it; an added field does not.
const FORMAT_VERSION: u32 = 1;

fn json<T: serde::Serialize>(value: T) -> Value {
    serde_json::to_value(value).expect("builtin catalog field serializes")
}

fn record<const N: usize>(fields: [(&str, Value); N]) -> Value {
    fields
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect::<Map<String, Value>>()
        .into()
}

pub(crate) fn builtin_catalog() -> Value {
    let registry = BuiltinRegistry::standard_ref();
    let mut functions: Vec<&BuiltinFunction> = registry.functions().collect();
    functions.sort_by_key(|function| function.name);
    let mut constants: Vec<&BuiltinConstant> = registry.constants().collect();
    constants.sort_by_key(|constant| constant.name);
    let mut series = BuiltinSeries::ALL.to_vec();
    series.sort_by_key(|series| series.name());
    let mut drawing_functions: Vec<_> = drawing_function_surface().iter().collect();
    drawing_functions.sort_by(|left, right| left.name.cmp(&right.name));
    let mut drawing_series: Vec<_> = drawing_series_surface().iter().collect();
    drawing_series.sort_by(|left, right| left.name.cmp(&right.name));
    let mut casts = CAST_FUNCTION_NAMES.to_vec();
    casts.sort_unstable();
    let mut accessors: Vec<(String, &FpField)> = FOOTPRINT_FIELDS
        .iter()
        .map(|field| (format!("footprint.{}", field.name), field))
        .chain(
            VOLUME_ROW_FIELDS
                .iter()
                .map(|field| (format!("volume_row.{}", field.name), field)),
        )
        .collect();
    accessors.sort_by(|left, right| left.0.cmp(&right.0));

    record([
        ("format", json("pinewasm-builtin-catalog")),
        ("formatVersion", json(FORMAT_VERSION)),
        ("compilerVersion", json(crate::COMPILER_VERSION)),
        (
            "functions",
            functions.into_iter().map(function_entry).collect(),
        ),
        (
            "drawingFunctions",
            drawing_functions
                .into_iter()
                .map(|spec| {
                    let overloads: Vec<Value> = spec
                        .overloads
                        .iter()
                        .map(|overload| {
                            overload
                                .iter()
                                .enumerate()
                                .map(drawing_param_entry)
                                .collect()
                        })
                        .collect();
                    record([
                        ("name", json(&spec.name)),
                        ("overloads", json(overloads)),
                        ("returns", json(spec.ret.describe())),
                        (
                            "returnQualifier",
                            json(spec.return_qualifier.map(|qualifier| match qualifier {
                                DrawingReturnQualifier::Fixed(qualifier) => qualifier.describe(),
                                DrawingReturnQualifier::JoinArgumentsAtLeastSimple => {
                                    "join of arguments, at least simple"
                                }
                            })),
                        ),
                    ])
                })
                .collect(),
        ),
        (
            "series",
            series
                .into_iter()
                .map(|series| {
                    record([
                        ("name", json(series.name())),
                        ("type", json(series.value_type().describe())),
                        (
                            "qualifier",
                            json(series.current_read_qualifier().describe()),
                        ),
                        ("inputSource", json(series.is_input_source())),
                        ("priceSource", json(series.is_price_source())),
                        ("strategyOnly", json(series.is_strategy())),
                    ])
                })
                .collect(),
        ),
        (
            "drawingSeries",
            drawing_series
                .into_iter()
                .map(|spec| {
                    record([
                        ("name", json(&spec.name)),
                        ("type", json(spec.ty.describe())),
                        ("qualifier", json(spec.qualifier.describe())),
                    ])
                })
                .collect(),
        ),
        (
            "constants",
            constants.into_iter().map(constant_entry).collect(),
        ),
        (
            "casts",
            casts
                .into_iter()
                .map(|name| {
                    record([
                        ("name", json(name)),
                        ("parameters", json(["x"])),
                        (
                            "returns",
                            json(cast_result_type(name).map(|ty| ty.describe())),
                        ),
                    ])
                })
                .collect(),
        ),
        (
            "footprintAccessors",
            accessors
                .into_iter()
                .map(|(name, field)| {
                    record([
                        ("name", json(name)),
                        ("parameters", json(["id"])),
                        ("returns", json(field.ty.to_type().describe())),
                    ])
                })
                .collect(),
        ),
        // Declaration order is the positional order, so it is not sorted.
        ("strategyDeclaration", json(&STRATEGY_DECL_PARAMS[..])),
    ])
}

fn function_entry(function: &BuiltinFunction) -> Value {
    let mut tables = Map::new();
    if !function.params.is_empty() {
        tables.insert("registry".into(), registry_params(&function.params));
    }
    if let Some(engine) = &function.ta_engine {
        tables.insert(
            "taEngine".into(),
            engine.iter().enumerate().map(ta_param_entry).collect(),
        );
    }
    match function.kind {
        BuiltinFunctionKind::Collection(func) => {
            let (names, required_count) = func.surface_params();
            let (min_argc, max_argc) = func.argc_bounds();
            tables.insert(
                "collectionSurface".into(),
                record([
                    ("parameters", json(names)),
                    ("requiredCount", json(required_count)),
                    ("argcBounds", json([min_argc, max_argc])),
                ]),
            );
        }
        BuiltinFunctionKind::InputGeneric => {
            tables.insert(
                "genericInputSource".into(),
                registry_params(&generic_input_source_params(function)),
            );
        }
        BuiltinFunctionKind::Pure(func) => {
            let (min_argc, max_argc) = func.argc_bounds();
            tables.insert("pureArgcBounds".into(), json([min_argc, max_argc]));
        }
        _ => {}
    }
    if function.name == "timestamp" {
        // The call analyzer selects among these overloads by argument shape.
        tables.insert(
            "timestampOverloads".into(),
            record([
                ("utc", json(TIMESTAMP_UTC_PARAM_NAMES)),
                ("timezone", json(TIMESTAMP_TZ_PARAM_NAMES)),
                ("dateString", json(TIMESTAMP_DATE_STRING_PARAM_NAMES)),
            ]),
        );
    }
    let (kind, detail) = kind_tag(function.kind);
    record([
        ("name", json(function.name)),
        ("kind", json(kind)),
        ("detail", json(detail)),
        ("returns", json(function.ret.describe())),
        (
            "pureResult",
            json(function.pure_result.map(|rule| format!("{rule:?}"))),
        ),
        ("variadicMax", json(function.variadic_max)),
        ("tables", tables.into()),
    ])
}

fn registry_params(params: &[ParamSpec]) -> Value {
    params
        .iter()
        .enumerate()
        .map(|(position, param)| {
            record([
                ("name", json(param.name)),
                ("position", json(position)),
                ("type", json(param.ty.describe())),
                ("typeRule", json(format!("{:?}", param.type_rule))),
                ("maxQualifier", json(param.max_qualifier.describe())),
                ("required", json(param.required)),
                (
                    "intLiteralRange",
                    json(param.int_literal_range.map(|(min, max)| [min, max])),
                ),
                (
                    "defaults",
                    record([
                        ("int", json(param.default_int)),
                        ("float", json(param.default_float)),
                        ("string", json(param.default_string)),
                        ("bool", json(param.default_bool)),
                        (
                            "series",
                            json(param.default_series.map(BuiltinSeries::name)),
                        ),
                    ]),
                ),
            ])
        })
        .collect()
}

fn ta_param_entry((position, param): (usize, &TaParam)) -> Value {
    let rule = match param.kind {
        TaParamKind::Source => record([("kind", json("source"))]),
        TaParamKind::SourceDefaulted(series) => record([
            ("kind", json("source")),
            ("defaultSeries", json(series.name())),
        ]),
        TaParamKind::Condition => record([("kind", json("condition"))]),
        TaParamKind::ConstInt { min, max, default } => record([
            ("kind", json("const int")),
            ("min", json(min)),
            ("max", json(max)),
            ("default", json(default)),
        ]),
        TaParamKind::ConstFloat { positive, optional } => record([
            ("kind", json("const float")),
            ("positive", json(positive)),
            ("optional", json(optional)),
        ]),
        TaParamKind::ConstPercent => record([("kind", json("const percent"))]),
        TaParamKind::ConstBool { default } => {
            record([("kind", json("const bool")), ("default", json(default))])
        }
        TaParamKind::ConstTimeframe { default } => record([
            ("kind", json("const timeframe")),
            ("defaultSeconds", json(default)),
        ]),
    };
    record([
        ("name", json(param.name)),
        ("position", json(position)),
        ("required", json(param.required())),
        ("rule", rule),
    ])
}

fn drawing_param_entry((position, param): (usize, &DrawingParameterSpec)) -> Value {
    record([
        ("name", json(&param.name)),
        ("position", json(position)),
        ("type", json(param.ty.describe())),
        ("required", json(param.required)),
        (
            "requiredQualifier",
            json(param.required_qualifier.map(Qualifier::describe)),
        ),
    ])
}

fn constant_entry(constant: &BuiltinConstant) -> Value {
    let value = if let Some(text) = constant.string_value {
        json(text)
    } else if let Some((r, g, b, t)) = constant.color_value {
        record([
            ("rgb", json(format!("#{r:02X}{g:02X}{b:02X}"))),
            ("transparency", json(t)),
        ])
    } else if let Some(int) = constant.int_value {
        json(int)
    } else if let Some(flag) = constant.bool_value {
        json(flag)
    } else {
        json(constant.value)
    };
    record([
        ("name", json(constant.name)),
        ("type", json(constant.ty.describe())),
        ("qualifier", json(constant.qualifier().describe())),
        ("value", value),
    ])
}

/// A stable kind name and the specific member it names, for every
/// [`BuiltinFunctionKind`]. The match is exhaustive so a new kind cannot be
/// published without a catalog name.
fn kind_tag(kind: BuiltinFunctionKind) -> (&'static str, Option<String>) {
    use BuiltinFunctionKind as K;
    match kind {
        K::Plot => ("plot", None),
        K::Indicator => ("indicator", None),
        K::MaxBarsBack => ("max_bars_back", None),
        K::Ta(func) => ("ta", Some(func.name().to_string())),
        K::Pure(func) => ("pure", Some(func.name().to_string())),
        K::Input(input) => ("input", Some(input.describe().to_string())),
        K::InputGeneric => ("input_generic", None),
        K::Collection(func) => ("collection", Some(func.name().to_string())),
        K::MapConstructor { key, val } => ("map_constructor", Some(format!("{key:?}, {val:?}"))),
        K::MatrixConstructor { elem } => ("matrix_constructor", Some(format!("{elem:?}"))),
        K::Log(level) => ("log", Some(level.member().to_string())),
        K::Alert => ("alert", None),
        K::AlertCondition => ("alertcondition", None),
        K::Display(display) => ("display", Some(display.name().to_string())),
        K::RuntimeErrorFn => ("runtime_error", None),
        K::Order(order) => ("order", Some(order.member().to_string())),
        K::Security => ("security", None),
        K::Request(request) => ("request", Some(request.member().to_string())),
        K::SecurityLowerTf => ("security_lower_tf", None),
        K::PivotPointLevels => ("pivot_point_levels", None),
        K::TimeFunction { close } => (
            "time_function",
            Some(if close { "time_close" } else { "time" }.to_string()),
        ),
    }
}
