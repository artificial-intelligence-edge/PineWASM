//! The builtin catalog reports each builtin table completely and in a stable
//! order.
use pinewasm_compiler::{builtin_catalog, BuiltinSeries, COMPILER_VERSION};
use pinewasm_hir::drawing::drawing_function_surface;
use pinewasm_semantics::BuiltinRegistry;
use serde_json::Value;

fn names(catalog: &Value, section: &str) -> Vec<String> {
    catalog[section]
        .as_array()
        .unwrap_or_else(|| panic!("`{section}` is an array"))
        .iter()
        .map(|entry| entry["name"].as_str().expect("named entry").to_string())
        .collect()
}

fn entry<'a>(catalog: &'a Value, section: &str, name: &str) -> &'a Value {
    catalog[section]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("`{name}` is in `{section}`"))
}

fn assert_sorted_and_unique(names: &[String], section: &str) {
    for pair in names.windows(2) {
        assert!(pair[0] < pair[1], "`{section}` out of order at {pair:?}");
    }
}

#[test]
fn header_identifies_the_layout_and_compiler() {
    let catalog = builtin_catalog();
    assert_eq!(catalog["format"], "pinewasm-builtin-catalog");
    assert_eq!(catalog["formatVersion"], 1);
    assert_eq!(catalog["compilerVersion"], COMPILER_VERSION);
}

#[test]
fn every_registry_table_entry_is_listed_once_in_name_order() {
    let catalog = builtin_catalog();
    let registry = BuiltinRegistry::standard_ref();

    let functions = names(&catalog, "functions");
    assert_sorted_and_unique(&functions, "functions");
    let mut expected: Vec<String> = registry.functions().map(|f| f.name.to_string()).collect();
    expected.sort();
    assert_eq!(functions, expected);

    let constants = names(&catalog, "constants");
    assert_sorted_and_unique(&constants, "constants");
    let mut expected: Vec<String> = registry.constants().map(|c| c.name.to_string()).collect();
    expected.sort();
    assert_eq!(constants, expected);

    let series = names(&catalog, "series");
    assert_sorted_and_unique(&series, "series");
    assert_eq!(series.len(), BuiltinSeries::ALL.len());

    let drawing = names(&catalog, "drawingFunctions");
    assert_sorted_and_unique(&drawing, "drawingFunctions");
    assert_eq!(drawing.len(), drawing_function_surface().len());
    for spec in drawing_function_surface() {
        let listed = entry(&catalog, "drawingFunctions", &spec.name);
        assert_eq!(
            listed["overloads"].as_array().unwrap().len(),
            spec.overloads.len(),
            "{}",
            spec.name
        );
    }
}

#[test]
fn the_catalog_is_deterministic() {
    assert_eq!(builtin_catalog().to_string(), builtin_catalog().to_string());
}

#[test]
fn each_builtin_series_has_exactly_one_value_type() {
    // `value_type` picks the first matching predicate, so the predicates must
    // be disjoint for every call site to type a builtin read the same way.
    for series in BuiltinSeries::ALL {
        let matches = [series.is_bool(), series.is_int(), series.is_string()]
            .iter()
            .filter(|flag| **flag)
            .count();
        assert!(matches <= 1, "{} has {matches} value types", series.name());
    }
}

#[test]
fn tables_are_reported_under_their_own_keys() {
    let catalog = builtin_catalog();

    // Registry rows keep their order, qualifier ceilings and requiredness.
    let plot = &entry(&catalog, "functions", "plot")["tables"]["registry"];
    assert_eq!(plot[0]["name"], "series");
    assert_eq!(plot[0]["maxQualifier"], "series");
    assert_eq!(plot[0]["required"], true);
    assert_eq!(plot[1]["name"], "title");
    assert_eq!(plot[1]["maxQualifier"], "const");
    assert_eq!(plot[1]["required"], false);
    assert!(plot[1]
        .as_object()
        .is_some_and(|row| row.contains_key("intLiteralRange") && row.contains_key("defaults")));

    // Collection calls publish both the registry rows and the call surface.
    let get = &entry(&catalog, "functions", "array.get")["tables"];
    assert_eq!(
        get["collectionSurface"]["parameters"],
        serde_json::json!(["id", "index"])
    );
    assert!(get["registry"].is_array());

    // The timestamp analyzer's overload tables travel with the registry row.
    let timestamp = &entry(&catalog, "functions", "timestamp")["tables"];
    assert_eq!(
        timestamp["timestampOverloads"]["dateString"][0],
        "dateString"
    );

    // Drawing calls live outside the registry and are listed separately.
    assert!(catalog["functions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|function| function["name"] != "line.new"));
    let line = entry(&catalog, "drawingFunctions", "line.new");
    assert_eq!(line["overloads"][0][0]["name"], "x1");
    assert_eq!(line["returnQualifier"], "series");
}

#[test]
fn casts_accessors_and_the_strategy_declaration_are_listed() {
    let catalog = builtin_catalog();

    let casts = names(&catalog, "casts");
    assert_sorted_and_unique(&casts, "casts");
    assert_eq!(casts.len(), 10);
    let float = entry(&catalog, "casts", "float");
    assert_eq!(float["parameters"], serde_json::json!(["x"]));
    assert_eq!(float["returns"], "float");
    assert_eq!(entry(&catalog, "casts", "line")["returns"], "line");

    let accessors = names(&catalog, "footprintAccessors");
    assert_sorted_and_unique(&accessors, "footprintAccessors");
    let delta = entry(&catalog, "footprintAccessors", "footprint.delta");
    assert_eq!(delta["parameters"], serde_json::json!(["id"]));
    assert_eq!(delta["returns"], "float");
    assert_eq!(
        entry(
            &catalog,
            "footprintAccessors",
            "volume_row.has_buy_imbalance"
        )["returns"],
        "bool"
    );

    // Declaration order is the positional order.
    let strategy = catalog["strategyDeclaration"].as_array().unwrap();
    assert_eq!(strategy[0], "title");
    assert_eq!(strategy[1], "shorttitle");
}

#[test]
fn series_and_constants_carry_their_types_and_qualifiers() {
    let catalog = builtin_catalog();
    let close = entry(&catalog, "series", "close");
    assert_eq!(close["type"], "float");
    assert_eq!(close["qualifier"], "series");
    assert_eq!(close["inputSource"], true);
    let tickerid = entry(&catalog, "series", "syminfo.tickerid");
    assert_eq!(tickerid["type"], "string");
    assert_eq!(tickerid["qualifier"], "simple");

    let sunday = entry(&catalog, "constants", "dayofweek.sunday");
    assert_eq!(sunday["type"], "int");
    assert_eq!(sunday["qualifier"], "const");
    assert_eq!(sunday["value"], 1);
}
