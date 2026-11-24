//! Nominal formatting and union rules for text format values, checked by
//! compile-time and runtime tests of PineWASM's own type rules.
use pinewasm_compiler::{
    compile, compile_and_run, rendering_contract, CompileOptions, MarketData, RunOptions,
    RunResult, Value,
};
use serde_json::{json, Value as Json};

const NONE: &str = "text.format_none";
const BOLD: &str = "text.format_bold";
const ITALIC: &str = "text.format_italic";
const BOTH: &str = "text.format_bold+text.format_italic";

fn source(body: &str) -> String {
    format!("//@version=6\nindicator(\"Nominal formatting\",overlay=true)\n{body}\n")
}

fn run(body: &str) -> RunResult {
    let market = MarketData {
        open: vec![10.0; 8],
        high: vec![12.0; 8],
        low: vec![9.0; 8],
        close: vec![11.0; 8],
        volume: vec![100.0; 8],
        timeframe: "1D".into(),
        ..MarketData::default()
    };
    let r = compile_and_run(
        &source(body),
        &market,
        CompileOptions::default(),
        &RunOptions::default(),
    );
    assert!(
        r.diagnostics.iter().all(|d| !d.is_error()),
        "{:?}",
        r.diagnostics
    );
    assert!(r.runtime_errors.is_empty(), "{:?}", r.runtime_errors);
    assert_eq!(r.bars_executed, 8);
    r
}

fn rejected(body: &str) {
    let r = compile(&source(body), CompileOptions::default());
    assert!(
        r.diagnostics
            .iter()
            .any(pinewasm_compiler::Diagnostic::is_error),
        "accepted {body}"
    );
}

fn plot_equals(value: &Value, expected: i64) -> bool {
    matches!(value, Value::Int(n) if *n==expected)
        || matches!(value, Value::Float(n) if *n==expected as f64)
}

#[test]
fn exported_formatting_fields_are_nominal() {
    let c = rendering_contract();
    assert_eq!(
        c["nominalScalarDomains"]["text_format"],
        json!([NONE, BOLD, ITALIC, BOTH])
    );
    for kind in ["label", "box"] {
        assert_eq!(
            c["drawingFields"][kind]["text_formatting"]["type"],
            json!("text_format"),
            "{kind}"
        );
    }
    assert_eq!(
        c["cellFields"]["text_formatting"]["type"],
        json!("text_format")
    );
}

#[test]
fn constant_algebra_folds_into_boolean_input_metadata() {
    let cases = [
        ("text.format_none+text.format_none==text.format_none", true),
        ("text.format_none+text.format_bold==text.format_bold", true),
        ("text.format_bold+text.format_none==text.format_bold", true),
        ("text.format_none+text.format_italic==text.format_italic", true),
        ("text.format_italic+text.format_none==text.format_italic", true),
        ("text.format_bold+text.format_italic==text.format_italic+text.format_bold", true),
        ("text.format_bold+text.format_bold==text.format_bold", true),
        ("text.format_italic+text.format_italic==text.format_italic", true),
        ("text.format_bold+text.format_italic+text.format_bold==text.format_bold+text.format_italic", true),
        ("text.format_bold+text.format_bold==text.format_italic", false),
        ("text.format_italic+text.format_italic==text.format_none", false),
        ("text.format_bold!=text.format_italic", true),
    ];
    let body = cases
        .iter()
        .enumerate()
        .map(|(i, (expr, _))| format!("b{i}=input.bool({expr},\"Case {i}\")\nplot(b{i}?1:0)\n"))
        .collect::<String>();
    let r = run(&body);
    let p = r.compile.as_ref().unwrap().program.as_ref().unwrap();
    for (i, (_, expected)) in cases.iter().enumerate() {
        assert_eq!(
            serde_json::to_value(&p.inputs[i].defval).unwrap(),
            json!({"Bool":expected}),
            "case {i}"
        );
        assert!(r.plots[i]
            .values
            .iter()
            .all(|v| plot_equals(v, i64::from(*expected))));
    }
}

#[test]
fn const_alias_union_remains_canonical_and_comparable() {
    let r=run("a=text.format_bold\nb=text.format_italic\nc=a+b+a+text.format_none\nplot(c==b+a?1:0)\nlabel.new(bar_index,close,\"F\",text_formatting=c)");
    assert!(r.plots[0].values.iter().all(|v| plot_equals(v, 1)));
    let d = serde_json::to_value(&r.drawings).unwrap();
    assert!(d["labels"]
        .as_array()
        .unwrap()
        .iter()
        .all(|l| l["fields"]["text_formatting"] == json!(BOTH)));
}

#[test]
fn series_union_produces_all_four_bounded_states() {
    let r=run("f=bar_index%2==0?text.format_bold:text.format_none\ng=bar_index%3==0?text.format_italic:text.format_none\nlabel.new(bar_index,close,\"F\",text_formatting=f+g+f)\nbox.new(bar_index,high,bar_index+1,low,text=\"F\",text_formatting=g+f+g)");
    let d = serde_json::to_value(&r.drawings).unwrap();
    for i in 0..8 {
        let expected = match (i % 2 == 0, i % 3 == 0) {
            (false, false) => NONE,
            (true, false) => BOLD,
            (false, true) => ITALIC,
            (true, true) => BOTH,
        };
        for family in ["labels", "boxes"] {
            assert_eq!(
                d[family][i]["fields"]["text_formatting"],
                json!(expected),
                "{family} bar {i}"
            );
        }
    }
}

#[test]
fn constructors_and_setters_preserve_exact_format_states() {
    for (expr, expected) in [
        ("text.format_none", NONE),
        ("text.format_bold", BOLD),
        ("text.format_italic", ITALIC),
        (
            "text.format_italic+text.format_bold+text.format_italic",
            BOTH,
        ),
    ] {
        let r=run(&format!("if barstate.islast\n    l=label.new(bar_index,close,\"L\",text_formatting={expr})\n    b=box.new(0,high,1,low,text=\"B\",text_formatting={expr})\n    t=table.new(position.top_left,1,1)\n    table.cell(t,0,0,\"T\",text_formatting={expr})\n    label.set_text_formatting(l,{expr})\n    b.set_text_formatting({expr})\n    table.cell_set_text_formatting(t,0,0,{expr})"));
        let d = serde_json::to_value(&r.drawings).unwrap();
        assert_eq!(d["labels"][0]["fields"]["text_formatting"], json!(expected));
        assert_eq!(d["boxes"][0]["fields"]["text_formatting"], json!(expected));
        assert_eq!(
            d["tables"][0]["cells"][0]["attrs"]["text_formatting"],
            json!(expected)
        );
    }
}

#[test]
fn nominal_format_constants_cannot_supply_string_inputs() {
    for expr in [
        "text.format_none",
        "text.format_bold",
        "text.format_italic",
        "text.format_bold+text.format_italic",
    ] {
        rejected(&format!("f=input.string({expr})\nplot(close)"));
    }
    rejected("f=text.format_bold\ng=input.string(f)\nplot(close)");
}

#[test]
fn nominal_formats_reject_mixed_string_and_foreign_enum_operators() {
    for expr in [
        "text.format_bold+\"\"",
        "\"\"+text.format_italic",
        "text.format_bold==\"text.format_bold\"",
        "text.format_bold+strategy.long",
        "text.format_bold==strategy.long",
        "text.format_bold-text.format_none",
        "text.format_bold*text.format_italic",
    ] {
        rejected(&format!("f={expr}\nplot(close)"));
    }
}

#[test]
fn constructor_formatting_rejects_literal_strings() {
    for call in [
        "label.new(bar_index,close,\"L\",text_formatting=\"text.format_bold\")",
        "box.new(0,high,1,low,text_formatting=\"text.format_bold\")",
        "table.cell(table.new(position.top_left,1,1),0,0,\"T\",text_formatting=\"text.format_bold\")",
    ] { rejected(&format!("{call}\nplot(close)")); }
}

#[test]
fn setters_reject_literal_format_strings() {
    for call in [
        "label.set_text_formatting(label.new(0,close),\"text.format_bold\")",
        "box.set_text_formatting(box.new(0,high,1,low),\"text.format_italic\")",
        "table.cell_set_text_formatting(table.new(position.top_left,1,1),0,0,\"text.format_none\")",
    ] {
        rejected(&format!("{call}\nplot(close)"));
    }
}

#[test]
fn direct_format_absence_is_not_an_accepted_argument() {
    for call in [
        "label.new(bar_index,close,\"L\",text_formatting=na)",
        "box.new(0,high,1,low,text_formatting=na)",
        "table.cell(table.new(position.top_left,1,1),0,0,\"T\",text_formatting=na)",
        "table.cell_set_text_formatting(table.new(position.top_left,1,1),0,0,na)",
    ] {
        rejected(&format!("{call}\nplot(close)"));
    }
}

#[test]
fn conditional_format_absence_and_reassignment_are_rejected() {
    rejected("f=bar_index%2==0?text.format_bold:na\nlabel.new(bar_index,close,\"L\",text_formatting=f)\nplot(close)");
    rejected("f=text.format_bold\nf:=na\nplot(close)");
    rejected("f=text.format_bold+na\nplot(close)");
}

#[test]
fn text_format_is_not_a_valid_explicit_type_keyword() {
    rejected("text_format f=text.format_bold\nplot(close)");
}

#[test]
fn builtin_formats_cannot_be_inferred_array_elements() {
    rejected("a=array.from(text.format_bold,text.format_italic)\nplot(array.size(a))");
}

#[test]
fn a_user_enum_named_text_format_stays_distinct_from_builtin_formatting() {
    let r =
        run("enum text_format\n    custom\nf=text_format.custom\nplot(f==text_format.custom?1:0)");
    assert!(r.plots[0].values.iter().all(|v| plot_equals(v, 1)));
    rejected("enum text_format\n    custom\nlabel.new(bar_index,close,\"L\",text_formatting=text_format.custom)\nplot(close)");
    rejected("enum text_format\n    custom\nf=text_format.custom+text.format_bold\nplot(close)");
}

#[test]
fn formatting_history_is_rejected_for_constant_series_and_compound_targets() {
    for body in [
        "label.new(bar_index,close,\"L\",text_formatting=text.format_bold[1])\nplot(close)",
        "f=bar_index%2==0?text.format_bold:text.format_none\nlabel.new(bar_index,close,\"L\",text_formatting=f[1])\nplot(close)",
        "f=bar_index%2==0?text.format_bold:text.format_none\nlabel.new(bar_index,close,\"L\",text_formatting=f[bar_index%2])\nplot(close)",
        "f=bar_index%2==0?text.format_bold:text.format_none\nlabel.new(bar_index,close,\"L\",text_formatting=(f+text.format_italic)[1])\nplot(close)",
    ] { rejected(body); }
}

#[test]
fn nominal_formatting_is_not_a_nullable_or_stringable_scalar() {
    for body in [
        "plot(na(text.format_bold)?1:0)",
        "label.new(bar_index,close,str.tostring(text.format_bold))\nplot(close)",
        "label.new(bar_index,close,str.tostring(text.format_bold+text.format_italic))\nplot(close)",
        "label.new(bar_index,close,string(text.format_bold))\nplot(close)",
    ] {
        rejected(body);
    }
}

#[test]
fn formatting_if_and_switch_results_require_an_explicit_default() {
    rejected("f=if bar_index%2==0\n    text.format_bold\nlabel.new(bar_index,close,\"L\",text_formatting=f)\nplot(close)");
    rejected("f=switch bar_index\n    0 => text.format_bold\nlabel.new(bar_index,close,\"L\",text_formatting=f)\nplot(close)");
    rejected("f=switch bar_index%2\n    0 => text.format_bold\n    => na\nlabel.new(bar_index,close,\"L\",text_formatting=f)\nplot(close)");
}

#[test]
fn tuple_control_flow_keeps_implicit_missing_values() {
    // PineWASM's compiler accepts these tuple declarations. The assertions check
    // that missing values are preserved at runtime.
    for declaration in [
        "[n,f]=if bar_index%2==0\n    [1,text.format_bold]",
        "[n,f]=switch bar_index\n    0 => [1,text.format_bold]",
    ] {
        let r=run(&format!("{declaration}\nlabel.new(bar_index,close,\"F\",text_formatting=f+text.format_italic)\nplot(n)"));
        let d = serde_json::to_value(&r.drawings).unwrap();
        for i in 0..8 {
            let present = if declaration.contains("switch") {
                i == 0
            } else {
                i % 2 == 0
            };
            assert_eq!(
                d["labels"][i]["fields"]["text_formatting"],
                if present { json!(BOTH) } else { Json::Null }
            );
        }
    }
}

#[test]
fn exhaustive_if_and_switch_preserve_nominal_series_formatting() {
    let r=run("f=if bar_index%2==0\n    text.format_bold\nelse\n    text.format_none\ng=switch bar_index%3\n    0 => text.format_italic\n    => text.format_none\nlabel.new(bar_index,close,\"F\",text_formatting=f+g+f)");
    let d = serde_json::to_value(&r.drawings).unwrap();
    for i in 0..8 {
        let expected = match (i % 2 == 0, i % 3 == 0) {
            (false, false) => NONE,
            (true, false) => BOLD,
            (false, true) => ITALIC,
            (true, true) => BOTH,
        };
        assert_eq!(d["labels"][i]["fields"]["text_formatting"], json!(expected));
    }
}

#[test]
fn ordinary_strings_keep_their_existing_concatenation() {
    let r =
        run("f=\"bold\"+\" italic\"\nplot(f==\"bold italic\"?1:0)\nlabel.new(bar_index,close,f)");
    assert!(r.plots[0].values.iter().all(|v| plot_equals(v, 1)));
    let d: Json = serde_json::to_value(&r.drawings).unwrap();
    assert_eq!(d["labels"][0]["fields"]["text"], json!("bold italic"));
    assert_eq!(d["labels"][0]["fields"]["text_formatting"], json!(NONE));
}
