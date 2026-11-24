//! Named collection binding regression coverage. Optional interior defaults,
//! runtime results and every generic instantiation remain separate obligations.
use pinewasm_compiler::{compile, CompileOptions};
use pinewasm_span::Severity;

fn errors(body: &str) -> Vec<String> {
    compile(
        &format!("//@version=6\nindicator(\"Collection binding\")\n{body}\nplot(close)\n"),
        CompileOptions::default(),
    )
    .diagnostics
    .into_iter()
    .filter(|d| d.severity == Severity::Error)
    .map(|d| d.message)
    .collect()
}

#[test]
fn named_collection_parameters_reorder_and_preserve_trailing_defaults() {
    for body in [
        "a = array.from(3, 1, 2)\narray.sort(order=order.ascending, id=a)",
        "a = array.from(3, 1, 2)\narray.sort(id=a)",
        "a = array.from(3, 1, 2)\na.sort(order=order.descending)",
        "a = array.new_float(initial_value=1.0, size=2)\narray.push(value=2.0, id=a)",
        "a = array.from(3, 1, 2)\nx = array.min(nth=1, id=a)",
        "a = array.from(3, 1, 2)\nx = array.stdev(biased=false, id=a)",
        "m = map.new<string, float>()\nmap.put(value=2.0, key=\"x\", id=m)\nx = map.get(key=\"x\", id=m)",
        "m = matrix.new<float>(2, 2, 0.0)\nmatrix.set(value=3.0, column=1, row=0, id=m)\nx = matrix.get(column=1, id=m, row=0)",
        "a = str.split(separator=\",\", string=\"a,b\")",
        "a = array.new_float(initial_value=1.0)",
        "a = array.from(3, 1, 2)\narray.fill(id=a, value=4, index_to=2)",
        "m = matrix.new<float>(2, 2, 0.0)\nmatrix.sort(id=m, order=order.descending)",
    ] {
        assert!(errors(body).is_empty(), "{body}: {:?}", errors(body));
    }
}

#[test]
fn named_collection_parameters_reject_unknown_duplicate_and_missing_names() {
    for call in [
        "array.sort(id=a, unknown=1)",
        "array.sort(id=a, id=a)",
        "array.sort(order=order.ascending)",
        "array.push(id=a)",
    ] {
        assert!(
            !errors(&format!("a = array.from(3, 1, 2)\n{call}")).is_empty(),
            "{call}"
        );
    }
}

#[test]
fn corrected_reference_names_compile_in_namespace_and_method_calls() {
    for call in [
        "array.concat(id2=b, id1=a)",
        "array.binary_search(val=2, id=a)",
        "array.binary_search_leftmost(val=2, id=a)",
        "array.binary_search_rightmost(val=2, id=a)",
        "a.binary_search(val=2)",
        "map.put_all(id2=n, id=m)",
        "matrix.concat(id2=y, id1=x)",
        "matrix.sum(id2=y, id1=x)",
        "matrix.diff(id2=y, id1=x)",
        "matrix.mult(id2=y, id1=x)",
        "matrix.kron(id2=y, id1=x)",
        "matrix.pow(power=2, id=x)",
        "x.pow(power=2)",
    ] {
        let body = format!("a = array.from(1, 2)\nb = array.from(3, 4)\nm = map.new<string, float>()\nn = map.new<string, float>()\nx = matrix.new<float>(2, 2, 1.0)\ny = matrix.new<float>(2, 2, 1.0)\n{call}");
        assert!(errors(&body).is_empty(), "{call}: {:?}", errors(&body));
    }
    for call in [
        "array.binary_search(id=a, value=2)",
        "array.concat(id=a, other=a)",
        "matrix.pow(id=x, exponent=2)",
    ] {
        let body = format!("a = array.from(1, 2)\nx = matrix.new<float>(2, 2, 1.0)\n{call}");
        assert!(
            !errors(&body).is_empty(),
            "accepted unknown parameter: {call}"
        );
    }
}

#[test]
fn array_join_accepts_reference_element_types_and_rejects_other_arrays() {
    for elements in ["1, -2, 3", "1.25, -2.5, 3.0", "\"a\", \"b\""] {
        for call in [
            "array.join(a)",
            "array.join(separator=\"|\", id=a)",
            "a.join(\"|\")",
        ] {
            let body = format!("a = array.from({elements})\nx = {call}");
            assert!(errors(&body).is_empty(), "{body}: {:?}", errors(&body));
        }
    }
    for constructor in [
        "array.new_bool()",
        "array.new_color()",
        "array.new_line()",
        "array.new_label()",
    ] {
        let body = format!("a = {constructor}\nx = array.join(a)");
        assert!(!errors(&body).is_empty(), "unexpected acceptance: {body}");
    }
    assert!(!errors("a = array.from(1, 2)\nx = array.join(a, 3)").is_empty());
}
