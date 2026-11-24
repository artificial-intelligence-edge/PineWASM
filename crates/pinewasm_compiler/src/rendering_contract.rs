//! Rendering domains are read from the actual Serde models. The host consumes
//! this record rather than maintaining a second enum registry.
use serde::de::{self, DeserializeOwned, Visitor};
use serde::Deserializer;

#[derive(Default)]
struct Domains {
    values: Vec<&'static str>,
}

impl<'de> Deserializer<'de> for &mut Domains {
    type Error = de::value::Error;
    fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
        Err(de::Error::custom("domain discovery only"))
    }
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _: &'static str,
        values: &'static [&'static str],
        _: V,
    ) -> Result<V::Value, Self::Error> {
        self.values = values.to_vec();
        Err(de::Error::custom("domain captured"))
    }
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        fields: &'static [&'static str],
        _: V,
    ) -> Result<V::Value, Self::Error> {
        self.values = fields.to_vec();
        Err(de::Error::custom("fields captured"))
    }
    // Spell out forwarding so the API surface inventory can inspect every trait
    // member without an unmodeled macro expansion.
    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_i16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_u16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_u32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.deserialize_any(visitor)
    }
}

fn json<T: serde::Serialize>(value: T) -> serde_json::Value {
    serde_json::to_value(value).expect("rendering contract field serializes")
}

fn record<const N: usize>(fields: [(&str, serde_json::Value); N]) -> serde_json::Value {
    fields
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect::<serde_json::Map<String, serde_json::Value>>()
        .into()
}

fn domain<T: DeserializeOwned>() -> Vec<&'static str> {
    let mut capture = Domains::default();
    let _ = T::deserialize(&mut capture);
    let () = assert!(!capture.values.is_empty(), "expected a Serde enum");
    capture.values
}

/// `deserialize_struct` also reports accepted aliases. Remove those aliases
/// from serialized DTO manifests so the frontend validates the actual wire
/// names emitted by `Serialize`.
fn wire_fields<T: DeserializeOwned>(aliases: &[&str]) -> Vec<&'static str> {
    domain::<T>()
        .into_iter()
        .filter(|field| !aliases.contains(field))
        .collect()
}

/// Version one retains logical bar indexes, epoch-ms drawing times, native
/// `na`/null and stable registration IDs. Host-specific wire encodings are
/// deliberately outside this semantic boundary.
pub fn rendering_contract() -> serde_json::Value {
    use pinewasm_hir::drawing_options::{
        cell_option_domain, drawing_option_domain, DrawingOptionDomain,
    };
    use pinewasm_hir::{
        input_active_call_argc_bounds, ArrowRuntimeColorers, ArrowStaticColorOrigin,
        ArrowStaticColors, ArrowStaticRoleColor, BoundStyleScalar, BuiltinSeries, CellAttr,
        DisplayKind, DisplayStyle, DrawDefault, DrawKind, ExternalSourceRef, FillColorMode,
        InputActive, InputActiveBinaryOp, InputActiveDirect, InputActiveExpr, InputActiveUnaryOp,
        InputConstraints, InputDecl, InputDisplay, InputKind, InputValue, InputWidget, LogLevel,
        MarkerLocation, MarkerShape, MarkerSize, OhlcBodyColor, OhlcBodyStyle, OhlcField,
        PlotColorMode, PlotColorRole, PlotDisplay, PlotLineStyle, PlotStyleKind, Qualifier,
        RuntimeStyleScalar, StyleColorPaletteEntry, DEFAULT_PLOT_COLOR,
    };
    fn option_values(domain: DrawingOptionDomain) -> serde_json::Value {
        domain
            .members()
            .flat_map(|(member, value)| {
                [
                    (member.to_string(), json(member)),
                    (value.to_string(), json(member)),
                ]
            })
            .collect::<serde_json::Map<String, serde_json::Value>>()
            .into()
    }
    fn default_value(default: DrawDefault) -> serde_json::Value {
        match default {
            DrawDefault::Na => serde_json::Value::Null,
            DrawDefault::Bool(value) => json(value),
            DrawDefault::Int(value) => json(value),
            DrawDefault::Float(value) => json(value),
            DrawDefault::Str(value) => json(value),
            DrawDefault::Color(_) => {
                let (r, g, b) = default.color_channels().expect("color default");
                json(format!("#{r:02X}{g:02X}{b:02X}"))
            }
        }
    }
    let mut drawing_fields = serde_json::Map::new();
    let mut drawing_option_domains = serde_json::Map::new();
    for kind in DrawKind::all() {
        let options = kind
            .fields()
            .iter()
            .filter_map(|field| {
                drawing_option_domain(*kind, field.name)
                    .map(|domain| (field.name.to_string(), option_values(domain)))
            })
            .collect::<serde_json::Map<String, serde_json::Value>>();
        drawing_option_domains.insert(kind.namespace().to_string(), options.into());
        let fields: serde_json::Map<String, serde_json::Value> = kind
            .fields()
            .iter()
            .map(|field| {
                let default = default_value(field.default);
                (
                    field.name.to_string(),
                    record([
                        ("type", json(field.ty().describe())),
                        ("default", json(default)),
                    ]),
                )
            })
            .collect();
        drawing_fields.insert(kind.namespace().to_string(), fields.into());
    }
    let cell_fields: serde_json::Map<String, serde_json::Value> = CellAttr::all()
        .iter()
        .map(|attr| {
            (
                attr.name().to_string(),
                record([
                    ("type", json(attr.ty().describe())),
                    ("default", json(default_value(attr.default()))),
                ]),
            )
        })
        .collect();
    let defaults = serde_json::to_value(DisplayStyle::default()).expect("style serializes");
    let cell_option_domains = CellAttr::all()
        .iter()
        .filter_map(|attr| {
            cell_option_domain(*attr).map(|domain| (attr.name().to_string(), option_values(domain)))
        })
        .collect::<serde_json::Map<String, serde_json::Value>>();
    let registry = pinewasm_semantics::BuiltinRegistry::standard_ref();
    let constants: serde_json::Map<String, serde_json::Value> = registry
        .constants()
        .filter(|c| {
            c.string_value.is_some()
                && [
                    "plot.",
                    "hline.",
                    "shape.",
                    "location.",
                    "label.",
                    "line.",
                    "position.",
                    "size.",
                    "xloc.",
                    "yloc.",
                    "extend.",
                    "text.",
                    "font.",
                    "format.",
                    "scale.",
                    "display.",
                ]
                .iter()
                .any(|prefix| c.name.starts_with(prefix))
        })
        .map(|c| (c.name.to_string(), json(c.string_value.unwrap())))
        .collect();
    let binding_calls: serde_json::Map<String, serde_json::Value> = registry
        .functions()
        .filter_map(|f| {
            input_active_call_argc_bounds(f.name)
                .map(|bounds| (f.name.to_string(), json([bounds.0, bounds.1])))
        })
        .collect();
    // Keep the bodies of externally tagged InputValue variants and internally
    // tagged host expressions discoverable from the serialized models. The
    // frontend uses these lists to reject unknown nested fields at the v1
    // boundary without maintaining a second hand-written DTO schema.
    fn tagged_variant_fields(
        examples: Vec<(&str, serde_json::Value)>,
        external: bool,
    ) -> serde_json::Value {
        examples
            .into_iter()
            .map(|(variant, value)| {
                let fields = if external {
                    value
                        .get(variant)
                        .and_then(serde_json::Value::as_object)
                        .map(|object| object.keys().cloned().collect::<Vec<_>>())
                        .unwrap_or_default()
                } else {
                    value
                        .as_object()
                        .map(|object| {
                            object
                                .keys()
                                .filter(|name| name.as_str() != "kind")
                                .cloned()
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                };
                (variant.to_string(), json(fields))
            })
            .collect::<serde_json::Map<_, _>>()
            .into()
    }
    let input_value_fields = tagged_variant_fields(
        vec![
            ("Int", serde_json::to_value(InputValue::Int(1)).unwrap()),
            (
                "Float",
                serde_json::to_value(InputValue::Float(1.5)).unwrap(),
            ),
            (
                "Bool",
                serde_json::to_value(InputValue::Bool(true)).unwrap(),
            ),
            (
                "String",
                serde_json::to_value(InputValue::String("value".into())).unwrap(),
            ),
            (
                "Color",
                serde_json::to_value(InputValue::Color {
                    r: 1,
                    g: 2,
                    b: 3,
                    t: 4.5,
                })
                .unwrap(),
            ),
            (
                "Source",
                serde_json::to_value(InputValue::Source(BuiltinSeries::Close)).unwrap(),
            ),
            (
                "ExternalSource",
                serde_json::to_value(InputValue::ExternalSource(ExternalSourceRef {
                    script_id: "fixture".into(),
                    plot_slot: 0,
                    plot_label: "Close".into(),
                }))
                .unwrap(),
            ),
            (
                "Enum",
                serde_json::to_value(InputValue::Enum {
                    name: "Fixture".into(),
                    index: 0,
                    member: "first".into(),
                    title: Some("First".into()),
                })
                .unwrap(),
            ),
        ],
        true,
    );
    let input_active_expression_fields = tagged_variant_fields(
        vec![
            (
                "value",
                serde_json::to_value(InputActiveExpr::Value {
                    value: InputValue::Int(1),
                })
                .unwrap(),
            ),
            ("na", serde_json::to_value(InputActiveExpr::Na).unwrap()),
            (
                "input",
                serde_json::to_value(InputActiveExpr::Input { input: 0 }).unwrap(),
            ),
            (
                "unary",
                serde_json::to_value(InputActiveExpr::Unary {
                    op: InputActiveUnaryOp::Not,
                    operand: Box::new(InputActiveExpr::Na),
                })
                .unwrap(),
            ),
            (
                "binary",
                serde_json::to_value(InputActiveExpr::Binary {
                    op: InputActiveBinaryOp::Add,
                    lhs: Box::new(InputActiveExpr::Na),
                    rhs: Box::new(InputActiveExpr::Na),
                })
                .unwrap(),
            ),
            (
                "ternary",
                serde_json::to_value(InputActiveExpr::Ternary {
                    condition: Box::new(InputActiveExpr::Na),
                    when_true: Box::new(InputActiveExpr::Na),
                    when_false: Box::new(InputActiveExpr::Na),
                })
                .unwrap(),
            ),
            (
                "call",
                serde_json::to_value(InputActiveExpr::Call {
                    function: "math.max".into(),
                    args: vec![InputActiveExpr::Na],
                })
                .unwrap(),
            ),
        ],
        false,
    );
    let ta_input_target_fields = tagged_variant_fields(
        vec![
            (
                "Sma",
                serde_json::to_value(pinewasm_hir::TaInputTarget::Sma { slot: 0 }).unwrap(),
            ),
            (
                "Change",
                serde_json::to_value(pinewasm_hir::TaInputTarget::Change { slot: 0 }).unwrap(),
            ),
            (
                "State",
                serde_json::to_value(pinewasm_hir::TaInputTarget::State { state: 0, param: 0 })
                    .unwrap(),
            ),
        ],
        true,
    );
    record([
        (
            "schemaVersion",
            json(pinewasm_csil::RENDERING_CONTRACT_VERSION),
        ),
        (
            "nominalScalarDomains",
            record([(
                "text_format",
                json(pinewasm_hir::text_format::TEXT_FORMAT_VALUES),
            )]),
        ),
        (
            "coordinateUnits",
            record([
                ("index", json("logical-bar")),
                ("time", json("epoch-milliseconds")),
                ("price", json("script-value")),
            ]),
        ),
        ("drawingIdentity", json("run-local-registration-ordinal")),
        ("missingNumeric", json("null")),
        (
            "colorModel",
            record([
                (
                    "rgb",
                    record([("type", json("u8")), ("range", json([0, 255]))]),
                ),
                (
                    "transparency",
                    record([
                        ("type", json("f64")),
                        ("range", json([0, 100])),
                        ("wholeJson", json("integer")),
                        ("fractionJson", json("number")),
                    ]),
                ),
                (
                    "paintAlpha",
                    record([
                        ("bits", json(8)),
                        ("normalizationDecimalPlaces", json(4)),
                        ("rounding", json("nearest-half-up")),
                    ]),
                ),
                ("accessorResult", json("float")),
                ("nativeTransparency", json("proven-whole-or-reference")),
            ]),
        ),
        (
            "integerWire",
            record([
                ("domain", json("signed-i64")),
                ("safe", json("number")),
                ("unsafe", json("canonical-decimal-string")),
                ("presentation", json("f64-coordinates-only")),
            ]),
        ),
        ("styleDefaults", json(defaults)),
        ("constants", json(constants)),
        ("bindingCalls", json(binding_calls)),
        (
            "bindingFields",
            json(pinewasm_hir::DISPLAY_STYLE_BINDING_FIELDS),
        ),
        (
            "bindingQualifiers",
            json(
                pinewasm_hir::DISPLAY_STYLE_BINDING_FIELDS
                    .iter()
                    .map(|field| {
                        (
                            (*field).to_string(),
                            serde_json::to_value(
                                pinewasm_hir::display_style_binding_qualifier(field).unwrap(),
                            )
                            .unwrap(),
                        )
                    })
                    .collect::<serde_json::Map<String, serde_json::Value>>(),
            ),
        ),
        (
            "colorRoleValueDomains",
            json(
                domain::<pinewasm_hir::PlotColorRole>()
                    .into_iter()
                    .map(|role| {
                        let parsed: pinewasm_hir::PlotColorRole =
                            serde_json::from_value(json(role)).unwrap();
                        (role.to_string(), json(parsed.value_domain()))
                    })
                    .collect::<serde_json::Map<String, serde_json::Value>>(),
            ),
        ),
        (
            "colorRolesByKind",
            json(
                domain::<DisplayKind>()
                    .into_iter()
                    .map(|kind| {
                        let parsed: DisplayKind = serde_json::from_value(json(kind)).unwrap();
                        (
                            kind.to_string(),
                            serde_json::to_value(parsed.color_roles()).unwrap(),
                        )
                    })
                    .chain(std::iter::once((
                        "fill".to_string(),
                        serde_json::to_value(pinewasm_hir::FILL_COLOR_ROLES).unwrap(),
                    )))
                    .collect::<serde_json::Map<String, serde_json::Value>>(),
            ),
        ),
        ("drawingFields", json(drawing_fields)),
        ("cellFields", json(cell_fields)),
        ("drawingOptionDomains", json(drawing_option_domains)),
        ("cellOptionDomains", json(cell_option_domains)),
        (
            "dtoFields",
            record([
                (
                    "DrawingObject",
                    json(domain::<pinewasm_vm::DrawingObject>()),
                ),
                ("Drawings", json(domain::<pinewasm_vm::Drawings>())),
                ("LogEntry", json(domain::<pinewasm_vm::LogEntry>())),
                ("AlertEvent", json(domain::<pinewasm_vm::AlertEvent>())),
                ("VmProfile", json(domain::<pinewasm_vm::VmProfile>())),
                ("RunResult", json(domain::<crate::RunResult>())),
                (
                    "CompileResult",
                    json(wire_fields::<crate::CompileResult>(&["csilText"])),
                ),
                (
                    "PhaseTimings",
                    json(wire_fields::<crate::PhaseTimings>(&["csilMs"])),
                ),
                ("CsilProgram", json(domain::<pinewasm_csil::CsilProgram>())),
                (
                    "CsilHeader",
                    json(wire_fields::<pinewasm_csil::CsilHeader>(&["csil_version"])),
                ),
                (
                    "TaInputBinding",
                    json(domain::<pinewasm_hir::TaInputBinding>()),
                ),
                (
                    "StrategyOutcome",
                    json(domain::<pinewasm_vm::StrategyOutcome>()),
                ),
                (
                    "StrategyTrade",
                    json(domain::<pinewasm_vm::StrategyTrade>()),
                ),
                (
                    "StrategyPosition",
                    json(domain::<pinewasm_vm::StrategyPosition>()),
                ),
                (
                    "StrategyReportTradeHighlight",
                    json(domain::<pinewasm_vm::StrategyReportTradeHighlight>()),
                ),
                (
                    "StrategyReportOutliers",
                    json(domain::<pinewasm_vm::StrategyReportOutliers>()),
                ),
                (
                    "StrategyReportScopeAggregate",
                    json(domain::<pinewasm_vm::StrategyReportScopeAggregate>()),
                ),
                (
                    "StrategyReportAggregates",
                    json(domain::<pinewasm_vm::StrategyReportAggregates>()),
                ),
                ("TableCell", json(domain::<pinewasm_vm::TableCellDto>())),
                ("SourceSpan", json(domain::<pinewasm_span::Span>())),
                ("DiagnosticLabel", json(domain::<pinewasm_span::Label>())),
                ("Diagnostic", json(domain::<pinewasm_span::Diagnostic>())),
                ("RuntimeError", json(domain::<pinewasm_vm::RuntimeError>())),
                (
                    "PlotColorSeries",
                    json(domain::<pinewasm_vm::PlotColorSeries>()),
                ),
                ("PlotSeries", json(domain::<pinewasm_vm::PlotSeries>())),
                ("VisualSeries", json(domain::<pinewasm_vm::VisualSeries>())),
                ("FillSeries", json(domain::<pinewasm_vm::FillSeries>())),
                (
                    "PlotTickSample",
                    json(domain::<pinewasm_vm::PlotTickSample>()),
                ),
                ("DisplayStyle", json(domain::<DisplayStyle>())),
                ("OhlcBodyColor", json(domain::<OhlcBodyColor>())),
                ("OhlcBodyStyle", json(domain::<OhlcBodyStyle>())),
                ("ArrowStaticColors", json(domain::<ArrowStaticColors>())),
                (
                    "ArrowRuntimeColorers",
                    json(domain::<ArrowRuntimeColorers>()),
                ),
                (
                    "ArrowStaticRoleColor",
                    json(domain::<ArrowStaticRoleColor>()),
                ),
                ("PlotInfo", json(domain::<pinewasm_hir::PlotInfo>())),
                ("VisualInfo", json(domain::<pinewasm_hir::VisualInfo>())),
                ("FillInfo", json(domain::<pinewasm_hir::FillInfo>())),
                ("CsilPlot", json(domain::<pinewasm_csil::CsilPlot>())),
                ("CsilVisual", json(domain::<pinewasm_csil::CsilVisual>())),
                ("CsilFill", json(domain::<pinewasm_csil::CsilFill>())),
                (
                    "ProgramMetadata",
                    json(domain::<pinewasm_csil::ProgramMetadata>()),
                ),
                ("DrawingCaps", json(domain::<pinewasm_hir::DrawingCaps>())),
                (
                    "IndicatorInfo",
                    json(domain::<pinewasm_hir::IndicatorInfo>()),
                ),
                ("StrategyInfo", json(domain::<pinewasm_hir::StrategyInfo>())),
                ("RuntimeStyleScalar", json(domain::<RuntimeStyleScalar>())),
                ("BoundStyleScalar", json(domain::<BoundStyleScalar>())),
                (
                    "UnsupportedVisualOption",
                    json(domain::<pinewasm_hir::UnsupportedVisualOption>()),
                ),
                (
                    "StyleColorPaletteEntry",
                    json(domain::<StyleColorPaletteEntry>()),
                ),
                ("InputDecl", json(domain::<InputDecl>())),
                ("ExternalSourceRef", json(domain::<ExternalSourceRef>())),
                ("InputConstraints", json(domain::<InputConstraints>())),
                ("InputActive", json(domain::<InputActive>())),
                ("InputActiveDirect", json(domain::<InputActiveDirect>())),
                ("PolyPoint", json(domain::<pinewasm_vm::PolyPointDto>())),
            ]),
        ),
        (
            "optionalDtoFields",
            record([
                ("CompileResult", json(["timings"])),
                ("CsilProgram", json(["ta_input_bindings"])),
                (
                    "VmProfile",
                    json(["combinedDispatchCount", "combinedDispatchByPc"]),
                ),
            ]),
        ),
        (
            "programHeader",
            record([
                ("magic", json(pinewasm_csil::CSIL_MAGIC)),
                ("il_version", json(pinewasm_csil::CSIL_VERSION)),
                ("language_version", json(pinewasm_csil::LANGUAGE_VERSION)),
                (
                    "runtime_abi_version",
                    json(pinewasm_csil::RUNTIME_ABI_VERSION),
                ),
            ]),
        ),
        (
            "programRuntimeAbi",
            record([
                ("default", json(pinewasm_csil::RUNTIME_ABI_VERSION)),
                ("taInputBindings", json(pinewasm_csil::TA_INPUT_ABI_VERSION)),
                (
                    "distanceScans",
                    json(pinewasm_csil::INDEXED_SCAN_ABI_VERSION),
                ),
            ]),
        ),
        (
            "taggedFields",
            record([
                ("InputValue", json(input_value_fields)),
                ("InputActiveExpr", json(input_active_expression_fields)),
                ("TaInputTarget", json(ta_input_target_fields)),
            ]),
        ),
        (
            "runtimeScalarQualifiers",
            json(
                domain::<Qualifier>()
                    .into_iter()
                    .filter_map(|value| {
                        let qualifier: Qualifier = serde_json::from_value(json(value)).ok()?;
                        (qualifier <= Qualifier::Simple).then_some(value)
                    })
                    .collect::<Vec<_>>(),
            ),
        ),
        (
            "optionalStyleFields",
            json([
                "sourceOrder",
                "fillGaps",
                "fillColorMode",
                "plotColorMode",
                "arrowStaticColors",
                "arrowRuntimeColorers",
                "ohlcBodyStyle",
                "boundScalars",
                "runtimeScalars",
                "colorPalette",
                "colorEditable",
            ]),
        ),
        (
            "automaticColorDefaults",
            record([
                ("editable", json(true)),
                ("palette", serde_json::Value::Array(Vec::new())),
            ]),
        ),
        (
            "outputColorDefaults",
            record([("plot", record([("color", json(DEFAULT_PLOT_COLOR))]))]),
        ),
        (
            "enums",
            record([
                ("DisplayKind", json(domain::<DisplayKind>())),
                ("PlotStyleKind", json(domain::<PlotStyleKind>())),
                ("PlotLineStyle", json(domain::<PlotLineStyle>())),
                ("PlotDisplay", json(domain::<PlotDisplay>())),
                ("OhlcField", json(domain::<OhlcField>())),
                ("MarkerLocation", json(domain::<MarkerLocation>())),
                ("MarkerShape", json(domain::<MarkerShape>())),
                ("MarkerSize", json(domain::<MarkerSize>())),
                ("PlotColorRole", json(domain::<PlotColorRole>())),
                ("FillColorMode", json(domain::<FillColorMode>())),
                ("PlotColorMode", json(domain::<PlotColorMode>())),
                (
                    "ArrowStaticColorOrigin",
                    json(domain::<ArrowStaticColorOrigin>()),
                ),
                ("LogLevel", json(domain::<LogLevel>())),
                (
                    "RuntimeErrorKind",
                    json(domain::<pinewasm_vm::RuntimeErrorKind>()),
                ),
                (
                    "DiagnosticSeverity",
                    json(domain::<pinewasm_span::Severity>()),
                ),
                ("InputKind", json(domain::<InputKind>())),
                ("InputWidget", json(domain::<InputWidget>())),
                ("InputDisplay", json(domain::<InputDisplay>())),
                ("InputActiveUnaryOp", json(domain::<InputActiveUnaryOp>())),
                ("InputActiveBinaryOp", json(domain::<InputActiveBinaryOp>())),
            ]),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn role_value_domains_cover_every_declared_lane() {
        let contract = rendering_contract();
        let domains = contract["colorRoleValueDomains"].as_object().unwrap();
        assert_eq!(domains.len(), domain::<pinewasm_hir::PlotColorRole>().len());
        for roles in contract["colorRolesByKind"].as_object().unwrap().values() {
            for role in roles.as_array().unwrap() {
                let name = role.as_str().unwrap();
                let expected = if matches!(name, "top_value" | "bottom_value") {
                    "float-or-na"
                } else {
                    "color-or-na"
                };
                assert_eq!(domains[name], expected, "{name}");
            }
        }
    }

    #[test]
    fn profile_optional_manifest_matches_serde_legacy_defaults() {
        let contract = rendering_contract();
        assert_eq!(
            contract["optionalDtoFields"]["VmProfile"],
            serde_json::json!(["combinedDispatchCount", "combinedDispatchByPc"])
        );
        let optional = contract["optionalDtoFields"]["VmProfile"]
            .as_array()
            .unwrap();
        let profile = pinewasm_vm::VmProfile {
            span_source: vec![0],
            span_start: vec![0],
            span_end: vec![1],
            count: vec![3.0],
            combined_dispatch_count: 1.0,
            combined_dispatch_by_pc: vec![1.0],
            units: vec![3.0],
            flow: vec![0],
        };
        let wire = serde_json::to_value(&profile).unwrap();
        for field in contract["dtoFields"]["VmProfile"].as_array().unwrap() {
            let mut missing = wire.clone();
            missing
                .as_object_mut()
                .unwrap()
                .remove(field.as_str().unwrap());
            let parsed = serde_json::from_value::<pinewasm_vm::VmProfile>(missing);
            assert_eq!(parsed.is_ok(), optional.contains(field), "{field}");
            let mut null = wire.clone();
            null.as_object_mut()
                .unwrap()
                .insert(field.as_str().unwrap().to_owned(), serde_json::Value::Null);
            assert!(serde_json::from_value::<pinewasm_vm::VmProfile>(null).is_err());
        }

        let mut legacy = wire.clone();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("combinedDispatchByPc");
        let legacy: pinewasm_vm::VmProfile = serde_json::from_value(legacy).unwrap();
        assert_eq!(legacy.combined_dispatch_count, 1.0);
        assert!(legacy.combined_dispatch_by_pc.is_empty());
        let round_trip = serde_json::to_value(legacy).unwrap();
        assert_eq!(round_trip["combinedDispatchByPc"], serde_json::json!([]));
        assert_eq!(round_trip["combinedDispatchCount"], 1.0);
        assert!(serde_json::from_value::<pinewasm_vm::VmProfile>(round_trip).is_ok());

        let mut older = wire;
        for field in optional {
            older
                .as_object_mut()
                .unwrap()
                .remove(field.as_str().unwrap());
        }
        let older: pinewasm_vm::VmProfile = serde_json::from_value(older).unwrap();
        assert_eq!(older.combined_dispatch_count, 0.0);
        assert!(older.combined_dispatch_by_pc.is_empty());
        assert_eq!(older.count, profile.count);
        assert_eq!(older.units, profile.units);
    }

    #[test]
    fn unsupported_visual_option_manifest_preserves_required_diagnostics() {
        let contract = rendering_contract();
        assert_eq!(
            contract["dtoFields"]["UnsupportedVisualOption"],
            serde_json::json!(["field", "reason"])
        );
        assert!(contract["optionalDtoFields"]
            .get("UnsupportedVisualOption")
            .is_none());
        let warning = pinewasm_hir::UnsupportedVisualOption {
            field: "offset".into(),
            reason: "value is outside the portable display contract's 32-bit range".into(),
        };
        let wire = serde_json::to_value(warning).unwrap();
        for field in ["field", "reason"] {
            let mut missing = wire.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<pinewasm_hir::UnsupportedVisualOption>(missing).is_err()
            );
        }
    }

    #[test]
    fn source_reference_manifest_preserves_lane_identity_fields() {
        let contract = rendering_contract();
        assert_eq!(
            contract["dtoFields"]["ExternalSourceRef"],
            serde_json::json!(["scriptId", "plotSlot", "plotLabel"])
        );
        let source = pinewasm_hir::ExternalSourceRef {
            script_id: "sample-script".into(),
            plot_slot: u32::MAX,
            plot_label: String::new(),
        };
        assert!(source.is_valid());
        let wire = serde_json::to_value(source).unwrap();
        for field in ["scriptId", "plotSlot", "plotLabel"] {
            let mut missing = wire.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<pinewasm_hir::ExternalSourceRef>(missing).is_err());
        }
    }

    #[test]
    fn discovered_domains_are_encoded_by_the_same_models() {
        let contract = rendering_contract();
        assert_eq!(contract["schemaVersion"], 1);
        assert!(contract["enums"]["MarkerLocation"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("in_bar")));
        for token in contract["enums"]["PlotDisplay"].as_array().unwrap() {
            let value: pinewasm_hir::PlotDisplay = serde_json::from_value(token.clone()).unwrap();
            assert_eq!(serde_json::to_value(value).unwrap(), *token);
        }
        let style: pinewasm_hir::DisplayStyle =
            serde_json::from_value(contract["styleDefaults"].clone()).unwrap();
        assert_eq!(style, pinewasm_hir::DisplayStyle::default());
    }

    #[test]
    fn compiler_style_scalar_manifests_cover_exact_fields_and_fixed_qualifiers() {
        let contract = rendering_contract();
        let fields = |name: &str| -> std::collections::BTreeSet<_> {
            contract["dtoFields"][name]
                .as_array()
                .unwrap()
                .iter()
                .map(|field| field.as_str().unwrap())
                .collect()
        };
        assert_eq!(
            fields("BoundStyleScalar"),
            ["expression", "field"].into_iter().collect()
        );
        assert_eq!(
            fields("RuntimeStyleScalar"),
            ["field", "qualifier", "slot"].into_iter().collect()
        );
        assert_eq!(
            fields("PlotInfo"),
            ["color_roles", "slot", "span", "style", "title"]
                .into_iter()
                .collect()
        );
        assert_eq!(
            fields("VisualInfo"),
            ["color_roles", "slot", "span", "style", "title"]
                .into_iter()
                .collect()
        );
        assert_eq!(
            fields("FillInfo"),
            [
                "color_roles",
                "plot1",
                "plot2",
                "slot",
                "span",
                "style",
                "title"
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            fields("CsilPlot"),
            ["color_roles", "style", "title"].into_iter().collect()
        );
        assert_eq!(
            fields("CsilVisual"),
            ["color_roles", "style", "title"].into_iter().collect()
        );
        assert_eq!(
            fields("CsilFill"),
            ["color_roles", "plot1", "plot2", "style", "title"]
                .into_iter()
                .collect()
        );
        assert_eq!(
            fields("ProgramMetadata"),
            [
                "drawing_caps",
                "indicator",
                "max_bars_back",
                "overlay",
                "rendering_contract_version",
                "strategy",
                "title"
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            fields("DrawingCaps"),
            [
                "max_boxes_count",
                "max_labels_count",
                "max_lines_count",
                "max_polylines_count"
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            fields("IndicatorInfo"),
            [
                "behind_chart",
                "calc_on_every_tick",
                "explicit_plot_zorder",
                "format",
                "max_bars_back",
                "max_boxes_count",
                "max_labels_count",
                "max_lines_count",
                "max_polylines_count",
                "overlay",
                "precision",
                "scale",
                "shorttitle",
                "span",
                "title"
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            fields("StrategyInfo"),
            [
                "alert_message",
                "backtest_fill_limits_assumption",
                "behind_chart",
                "calc_bars_count",
                "calc_on_every_history_tick",
                "calc_on_every_tick",
                "calc_on_order_fills",
                "close_entries_rule",
                "commission_type",
                "commission_value",
                "currency",
                "default_qty_type",
                "default_qty_value",
                "dynamic_requests",
                "explicit_plot_zorder",
                "fill_orders_on_standard_ohlc",
                "format",
                "initial_capital",
                "margin_long",
                "margin_short",
                "max_bars_back",
                "max_boxes_count",
                "max_labels_count",
                "max_lines_count",
                "max_polylines_count",
                "overlay",
                "precision",
                "process_orders_on_close",
                "pyramiding",
                "risk_free_rate",
                "scale",
                "slippage",
                "span",
                "title",
                "shorttitle",
                "use_bar_magnifier",
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            contract["runtimeScalarQualifiers"],
            serde_json::json!(["Const", "Input", "Simple"])
        );
    }

    #[test]
    fn run_result_and_strategy_manifests_cover_the_serialized_output_surface() {
        let contract = rendering_contract();
        let fields = |name: &str| -> std::collections::BTreeSet<_> {
            contract["dtoFields"][name]
                .as_array()
                .unwrap()
                .iter()
                .map(|field| field.as_str().unwrap())
                .collect()
        };
        assert_eq!(
            fields("RunResult"),
            [
                "alerts",
                "barsExecuted",
                "compile",
                "diagnostics",
                "drawings",
                "fills",
                "logs",
                "plots",
                "profile",
                "realtimeTrace",
                "runMs",
                "runtimeErrors",
                "strategy",
                "visuals",
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            fields("Drawings"),
            [
                "boxes",
                "labels",
                "linefills",
                "lines",
                "polylines",
                "tables"
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            fields("StrategyPosition"),
            ["avgPrice", "size"].into_iter().collect()
        );
        assert_eq!(
            fields("StrategyTrade"),
            [
                "comment",
                "commission",
                "cumulativePnl",
                "entryBar",
                "entryComment",
                "entryId",
                "entryNotional",
                "entryPrice",
                "exitBar",
                "exitId",
                "exitOrderName",
                "exitPrice",
                "index",
                "maxDrawdown",
                "maxRunup",
                "pnl",
                "qty",
                "sign",
            ]
            .into_iter()
            .collect()
        );
        assert!(!fields("StrategyTrade").contains("entryOrderOrdinal"));
        assert!(!fields("StrategyTrade").contains("exitOrderOrdinal"));
        assert_eq!(
            contract["enums"]["LogLevel"],
            serde_json::json!(["info", "warning", "error"])
        );
    }

    #[test]
    fn compile_and_program_manifests_cover_the_rendering_boundary() {
        let contract = rendering_contract();
        let fields = |name: &str| -> std::collections::BTreeSet<_> {
            contract["dtoFields"][name]
                .as_array()
                .unwrap()
                .iter()
                .map(|field| field.as_str().unwrap())
                .collect()
        };
        assert_eq!(
            fields("CompileResult"),
            [
                "astDebug",
                "diagnostics",
                "hirDebug",
                "ilText",
                "mirDebug",
                "program",
                "success",
                "timings",
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            fields("PhaseTimings"),
            [
                "dumpsMs",
                "ilMs",
                "librariesMs",
                "mirMs",
                "parseMs",
                "semanticsMs",
                "totalMs",
                "validateMs",
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            fields("CsilProgram"),
            [
                "constants",
                "fills",
                "functions",
                "header",
                "inputs",
                "metadata",
                "object_types",
                "plots",
                "series_slots",
                "ta_input_bindings",
                "ta_states",
                "visuals",
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            fields("CsilHeader"),
            [
                "il_version",
                "language_version",
                "magic",
                "runtime_abi_version",
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            fields("TaInputBinding"),
            ["input", "negated", "target"].into_iter().collect()
        );
        assert_eq!(
            contract["optionalDtoFields"],
            serde_json::json!({
                "CompileResult": ["timings"],
                "CsilProgram": ["ta_input_bindings"],
                "VmProfile": ["combinedDispatchCount", "combinedDispatchByPc"]
            })
        );
        assert_eq!(
            contract["programHeader"],
            serde_json::json!({
                "magic": pinewasm_csil::CSIL_MAGIC,
                "il_version": pinewasm_csil::CSIL_VERSION,
                "language_version": pinewasm_csil::LANGUAGE_VERSION,
                "runtime_abi_version": pinewasm_csil::RUNTIME_ABI_VERSION
            })
        );
        assert_eq!(
            contract["programRuntimeAbi"],
            serde_json::json!({
                "default": pinewasm_csil::RUNTIME_ABI_VERSION,
                "taInputBindings": pinewasm_csil::TA_INPUT_ABI_VERSION,
                "distanceScans": pinewasm_csil::INDEXED_SCAN_ABI_VERSION
            })
        );
        assert_eq!(
            contract["taggedFields"]["TaInputTarget"],
            serde_json::json!({
                "Sma": ["slot"],
                "Change": ["slot"],
                "State": ["param", "state"]
            })
        );

        let compiled = crate::compile(
            "indicator(\"render DTO contract\")\nplot(close)",
            crate::CompileOptions::default(),
        );
        assert!(compiled.success);
        let serialized = serde_json::to_value(compiled).unwrap();
        assert_eq!(
            serialized["program"]["header"]["runtime_abi_version"],
            pinewasm_csil::RUNTIME_ABI_VERSION
        );
        for field in fields("CompileResult") {
            assert!(serialized.get(field).is_some(), "missing compile.{field}");
        }
        let without_timings = crate::CompileResult {
            success: true,
            diagnostics: Vec::new(),
            ast_debug: None,
            hir_debug: None,
            mir_debug: None,
            csil_text: None,
            program: serde_json::from_value(serialized["program"].clone()).unwrap(),
            timings: None,
        };
        assert!(serde_json::to_value(without_timings)
            .unwrap()
            .get("timings")
            .is_none());
        let timings = serde_json::to_value(crate::PhaseTimings::default()).unwrap();
        for field in fields("PhaseTimings") {
            assert!(timings.get(field).is_some(), "missing timing.{field}");
        }
        let program = serialized["program"].as_object().unwrap();
        for field in fields("CsilProgram") {
            if field == "ta_input_bindings" {
                assert!(program.get(field).is_none());
            } else {
                assert!(program.get(field).is_some(), "missing program.{field}");
            }
        }

        let input_backed_ta = crate::compile(
            "indicator(\"input-backed TA ABI\")\nlength = input.int(10)\nplot(ta.ema(close, length))",
            crate::CompileOptions::default(),
        );
        assert!(input_backed_ta.success, "{:?}", input_backed_ta.diagnostics);
        let input_backed_ta = serde_json::to_value(input_backed_ta).unwrap();
        assert!(input_backed_ta["program"]["ta_input_bindings"]
            .as_array()
            .is_some_and(|bindings| !bindings.is_empty()));
        assert_eq!(
            input_backed_ta["program"]["header"]["runtime_abi_version"],
            pinewasm_csil::TA_INPUT_ABI_VERSION
        );
    }

    #[test]
    fn tagged_field_manifest_covers_every_serde_variant_and_nested_field() {
        let contract = rendering_contract();
        let input_value_variants = domain::<pinewasm_hir::InputValue>();
        let actual: std::collections::BTreeSet<_> = contract["taggedFields"]["InputValue"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let expected: std::collections::BTreeSet<_> = input_value_variants.into_iter().collect();
        assert_eq!(actual, expected, "stale InputValue variant manifest");

        let expression_variants = [
            pinewasm_hir::InputActiveExpr::Value {
                value: pinewasm_hir::InputValue::Bool(true),
            },
            pinewasm_hir::InputActiveExpr::Na,
            pinewasm_hir::InputActiveExpr::Input { input: 0 },
            pinewasm_hir::InputActiveExpr::Unary {
                op: pinewasm_hir::InputActiveUnaryOp::Not,
                operand: Box::new(pinewasm_hir::InputActiveExpr::Na),
            },
            pinewasm_hir::InputActiveExpr::Binary {
                op: pinewasm_hir::InputActiveBinaryOp::Add,
                lhs: Box::new(pinewasm_hir::InputActiveExpr::Na),
                rhs: Box::new(pinewasm_hir::InputActiveExpr::Na),
            },
            pinewasm_hir::InputActiveExpr::Ternary {
                condition: Box::new(pinewasm_hir::InputActiveExpr::Na),
                when_true: Box::new(pinewasm_hir::InputActiveExpr::Na),
                when_false: Box::new(pinewasm_hir::InputActiveExpr::Na),
            },
            pinewasm_hir::InputActiveExpr::Call {
                function: "math.max".into(),
                args: vec![pinewasm_hir::InputActiveExpr::Na],
            },
        ];
        let names: std::collections::BTreeSet<_> = expression_variants
            .iter()
            .map(|variant| match variant {
                pinewasm_hir::InputActiveExpr::Value { .. } => "value",
                pinewasm_hir::InputActiveExpr::Na => "na",
                pinewasm_hir::InputActiveExpr::Input { .. } => "input",
                pinewasm_hir::InputActiveExpr::Unary { .. } => "unary",
                pinewasm_hir::InputActiveExpr::Binary { .. } => "binary",
                pinewasm_hir::InputActiveExpr::Ternary { .. } => "ternary",
                pinewasm_hir::InputActiveExpr::Call { .. } => "call",
            })
            .collect();
        let actual: std::collections::BTreeSet<_> = contract["taggedFields"]["InputActiveExpr"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(actual, names, "stale InputActiveExpr variant manifest");
        for (variant, expected) in [
            ("Color", vec!["b", "g", "r", "t"]),
            ("Enum", vec!["index", "member", "name", "title"]),
        ] {
            let actual: std::collections::BTreeSet<_> = contract["taggedFields"]["InputValue"]
                [variant]
                .as_array()
                .unwrap()
                .iter()
                .map(|field| field.as_str().unwrap())
                .collect();
            let expected = expected.into_iter().collect();
            assert_eq!(actual, expected, "stale InputValue::{variant} fields");
        }
    }
    #[test]
    fn color_lane_manifest_includes_selected_input_identity() {
        let contract = rendering_contract();
        assert_eq!(
            contract["dtoFields"]["PlotColorSeries"],
            serde_json::json!(["role", "values", "paletteIndices"])
        );
    }
}
