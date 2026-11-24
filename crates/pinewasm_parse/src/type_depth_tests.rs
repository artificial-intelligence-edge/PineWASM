//! A declared type never nests more `<...>` argument lists than
//! `MAX_TYPE_DEPTH`, wherever the source declares it.
//!
//! Collection type arguments are the one place a type recurses. A type can be
//! declared as a `type` field, a parameter, a method receiver or an annotated
//! variable, and every position reaches the same recursion, so every position
//! must refuse a deeper type with a diagnostic and then parse what follows.
//! The `T[]` sugar wraps the innermost element without recursing, so an
//! accepted type is at most one collection level taller than the cap.

use crate::parser::MAX_TYPE_DEPTH;
use crate::{parse, MAX_SOURCE_LEN};
use pinewasm_span::{codes, has_errors, Diagnostic};
use pinewasm_syntax::{AnnotationKind, FieldType, FieldTypeKind, Item, Program, Stmt};
use proptest::prelude::*;

const CAP: usize = MAX_TYPE_DEPTH as usize;

const TOO_DEEP: &str = "type is nested too deeply";

/// Far past the depth at which an unbounded descent runs out of stack, and
/// still short enough that every shape below fits the source size cap, so it
/// is the nesting cap that refuses it.
const VERY_DEEP: usize = 60_000;

/// Where a declared type can be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Position {
    Field,
    Parameter,
    Receiver,
    Variable,
    PersistentVariable,
    QualifiedVariable,
    VariableInBlock,
}

const POSITIONS: [Position; 7] = [
    Position::Field,
    Position::Parameter,
    Position::Receiver,
    Position::Variable,
    Position::PersistentVariable,
    Position::QualifiedVariable,
    Position::VariableInBlock,
];

/// A script that declares `ty` at `position` and ends with `after = 1`, the
/// statement that must still parse when the type is refused.
fn script(position: Position, ty: &str) -> String {
    let declaration = match position {
        Position::Field => format!("type Holder\n    {ty} held\n    int count\n"),
        Position::Parameter => format!("f({ty} held) => 1\n"),
        Position::Receiver => format!("method m({ty} held) => 1\n"),
        Position::Variable => format!("{ty} held = na\n"),
        Position::PersistentVariable => format!("var {ty} held = na\n"),
        Position::QualifiedVariable => format!("series {ty} held = na\n"),
        Position::VariableInBlock => format!("if true\n    {ty} held = na\n"),
    };
    format!("{declaration}after = 1\n")
}

/// Collection levels on the tallest path of `ty` (a scalar or a name is 0).
/// It uses an explicit stack, so measuring a tree never recurses.
fn levels(ty: &FieldType) -> usize {
    let mut tallest = 0;
    let mut pending = vec![(ty, 0usize)];
    while let Some((node, above)) = pending.pop() {
        match &node.kind {
            FieldTypeKind::Array(element) | FieldTypeKind::Matrix(element) => {
                pending.push((element, above + 1));
            }
            FieldTypeKind::Map(key, value) => {
                pending.push((key, above + 1));
                pending.push((value, above + 1));
            }
            FieldTypeKind::Scalar(_) | FieldTypeKind::Color | FieldTypeKind::Named(_) => {
                tallest = tallest.max(above);
            }
        }
    }
    tallest
}

fn annotation_levels(kind: &AnnotationKind) -> usize {
    match kind {
        AnnotationKind::Array(element) | AnnotationKind::Matrix(element) => 1 + levels(element),
        AnnotationKind::Map(key, value) => 1 + levels(key).max(levels(value)),
        AnnotationKind::Scalar(_) | AnnotationKind::Color | AnnotationKind::Named(_) => 0,
    }
}

fn declared_variable_levels(stmt: &Stmt) -> Option<usize> {
    match stmt {
        Stmt::Assign {
            name,
            annotation: Some(annotation),
            ..
        } if name.name == "held" => Some(annotation_levels(&annotation.kind)),
        _ => None,
    }
}

/// The collection levels of the type `script(position, ..)` declared, or
/// `None` when the declaration is not in the tree.
fn declared_levels(position: Position, program: &Program) -> Option<usize> {
    program
        .items
        .iter()
        .find_map(|item| match (position, item) {
            (Position::Field, Item::TypeDef(decl)) => decl
                .fields
                .iter()
                .find(|field| field.name.name == "held")
                .map(|field| levels(&field.ty)),
            (Position::Parameter, Item::FuncDef(def)) => def.params[0].ty.as_ref().map(levels),
            (Position::Receiver, Item::FuncDef(def)) => def.receiver.as_ref().map(levels),
            (
                Position::Variable | Position::PersistentVariable | Position::QualifiedVariable,
                Item::Stmt(stmt),
            ) => declared_variable_levels(stmt),
            (Position::VariableInBlock, Item::Stmt(Stmt::If(branch))) => {
                branch.then_branch.iter().find_map(declared_variable_levels)
            }
            _ => None,
        })
}

/// Whether the trailing `after = 1` made it into the tree.
fn parsed_the_statement_after(program: &Program) -> bool {
    program.items.iter().any(|item| {
        matches!(
            item,
            Item::Stmt(Stmt::Assign { name, .. }) if name.name == "after"
        )
    })
}

fn too_deep(diagnostics: &[Diagnostic]) -> usize {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message == TOO_DEEP)
        .count()
}

/// `depth` collection keywords nested around `element`: `open` is the text
/// up to and including the nested argument's position (`array<`,
/// `map<string, `), `close` what follows it (`>`, `, int>`).
fn nested(open: &str, element: &str, close: &str, depth: usize) -> String {
    format!("{}{element}{}", open.repeat(depth), close.repeat(depth))
}

/// The shapes one keyword can be nested in: `(open, close)`.
const NESTINGS: [(&str, &str); 4] = [
    ("array<", ">"),
    ("matrix<", ">"),
    // A map nests through its value ...
    ("map<string, ", ">"),
    // ... and, as far as the grammar goes, through its key.
    ("map<", ", int>"),
];

#[track_caller]
fn assert_accepted(position: Position, ty: &str, expected_levels: usize) {
    let result = parse(&script(position, ty));
    assert!(
        !has_errors(&result.diagnostics),
        "{position:?}: {:?}",
        result.diagnostics.first()
    );
    assert_eq!(
        declared_levels(position, &result.program),
        Some(expected_levels),
        "{position:?}"
    );
    assert!(parsed_the_statement_after(&result.program), "{position:?}");
}

#[track_caller]
fn assert_refused(position: Position, ty: &str) {
    let source = script(position, ty);
    assert!(source.len() <= MAX_SOURCE_LEN, "{} bytes", source.len());
    let result = parse(&source);
    // One diagnostic, and nothing else: the rest of the declaration is
    // skipped, not parsed as something it is not.
    assert_eq!(
        too_deep(&result.diagnostics),
        1,
        "{position:?}: {:?}",
        result.diagnostics.first()
    );
    assert_eq!(
        result.diagnostics.len(),
        1,
        "{position:?}: {:?}",
        result.diagnostics
    );
    assert_eq!(declared_levels(position, &result.program), None);
    assert!(parsed_the_statement_after(&result.program), "{position:?}");
}

#[test]
fn a_type_is_accepted_up_to_the_cap_and_no_further() {
    for position in POSITIONS {
        for (open, close) in NESTINGS {
            assert_accepted(position, &nested(open, "int", close, CAP), CAP);
            assert_refused(position, &nested(open, "int", close, CAP + 1));
        }
    }
}

#[test]
fn very_deep_types_are_refused() {
    for position in POSITIONS {
        for (open, close) in NESTINGS {
            assert_refused(position, &nested(open, "int", close, VERY_DEEP));
        }
    }
    // Unbalanced input takes the same path: the cap is met on the way down,
    // before any closing `>` is looked for.
    for position in POSITIONS {
        assert_refused(position, &"array<".repeat(VERY_DEEP));
        assert_refused(position, &"map<int, ".repeat(VERY_DEEP));
    }
}

#[test]
fn the_refusal_points_at_the_level_that_goes_too_far() {
    let ty = nested("array<", "int", ">", CAP + 1);
    let source = script(Position::Variable, &ty);
    let result = parse(&source);
    let diagnostic = &result.diagnostics[0];
    assert_eq!(diagnostic.code.as_deref(), Some(codes::E0110));
    // The label covers the keyword that opens level `CAP + 1`.
    let label = &diagnostic.labels[0];
    let start = "array<".len() * CAP;
    assert_eq!(
        (label.span.start as usize, label.span.end as usize),
        (start, start + "array".len())
    );
    assert!(
        label.message.contains(&CAP.to_string()),
        "{}",
        label.message
    );
}

#[test]
fn the_array_suffix_adds_one_level_without_nesting() {
    // `int[]` is an array of `int` written without an argument list, so it
    // does not count against the cap: the tree is one level taller than the
    // deepest type the cap admits, and no taller.
    for position in POSITIONS {
        assert_accepted(position, &nested("array<", "int[]", ">", CAP), CAP + 1);
        assert_refused(position, &nested("array<", "int[]", ">", CAP + 1));
    }
}

#[test]
fn nesting_is_counted_per_path_not_per_type() {
    // Both arguments of every map sit at the same depth, so a type with
    // thousands of collection keywords is fine while its paths are short.
    let mut wide = String::from("int");
    for _ in 0..12 {
        wide = format!("map<{wide}, {wide}>");
    }
    assert_eq!(wide.matches("map").count(), 4095);
    for position in POSITIONS {
        assert_accepted(position, &wide, 12);
    }

    // A deep key beside a deep value is as deep as the deeper of the two.
    let key = nested("array<", "int", ">", CAP - 1);
    let value = nested("matrix<", "float", ">", CAP - 1);
    let both = format!("map<{key}, {value}>");
    let one_too_many = format!("map<{key}, matrix<{value}>>");
    for position in POSITIONS {
        assert_accepted(position, &both, CAP);
        assert_refused(position, &one_too_many);
    }
}

#[test]
fn every_declaration_of_a_script_is_checked_on_its_own() {
    // The count restarts with each type: many declarations at the cap parse,
    // and a refused one does not make the next one start deeper.
    let at_cap = nested("array<", "int", ">", CAP);
    let over = nested("array<", "int", ">", CAP + 1);
    let mut source = String::new();
    for index in 0..40 {
        source.push_str(&format!("{at_cap} ok{index} = na\n"));
        source.push_str(&format!("{over} refused{index} = na\n"));
    }
    let result = parse(&source);
    assert_eq!(too_deep(&result.diagnostics), 40);
    assert_eq!(result.diagnostics.len(), 40);
    let declared: Vec<&str> = result
        .program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Stmt(Stmt::Assign { name, .. }) => Some(name.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(declared.len(), 40);
    assert!(declared.iter().all(|name| name.starts_with("ok")));
}

#[test]
fn a_comparison_that_starts_like_a_type_is_still_an_expression() {
    // `array`, `matrix` and `map` are ordinary names outside a declaration,
    // so a statement that opens with one of them and `<` is a comparison
    // unless a declaration follows. The type parse at statement start is an
    // attempt: when what it reads is not a declaration it must put the
    // cursor back and report nothing, whether the attempt failed part way
    // (the first three) or read a complete type (the last).
    for statement in [
        "array < 2 ? 1 : 0",
        "map < matrix ? 1 : 0",
        "array < array < 3",
        "matrix < low > high",
    ] {
        let result = parse(&format!("{statement}\nafter = 1\n"));
        assert!(
            result.diagnostics.is_empty(),
            "{statement}: {:?}",
            result.diagnostics
        );
        assert!(
            matches!(result.program.items[0], Item::Stmt(Stmt::Expr { .. })),
            "{statement}: {:?}",
            result.program.items[0]
        );
        assert!(parsed_the_statement_after(&result.program), "{statement}");
    }
    // With a name and `=` after it, the same opening is a declaration.
    let result = parse("matrix<low> high = na\n");
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    assert_eq!(
        declared_variable_levels_of("high", &result.program),
        Some(1)
    );
}

/// The collection levels of the annotated variable `name` at top level.
fn declared_variable_levels_of(name: &str, program: &Program) -> Option<usize> {
    program.items.iter().find_map(|item| match item {
        Item::Stmt(Stmt::Assign {
            name: declared,
            annotation: Some(annotation),
            ..
        }) if declared.name == name => Some(annotation_levels(&annotation.kind)),
        _ => None,
    })
}

/// A syntactically valid type with the depth it was built to.
#[derive(Clone, Debug)]
struct Declared {
    text: String,
    /// The deepest nesting of `<...>` argument lists: what the cap counts.
    lists: usize,
    /// Collection levels of the tree, the `[]` sugar included.
    levels: usize,
}

fn element(text: &str, levels: usize) -> Declared {
    Declared {
        text: text.to_string(),
        lists: 0,
        levels,
    }
}

/// Types with long runs of one collection keyword at every level, so that
/// many cases land on each side of the cap.
fn declared_type() -> impl Strategy<Value = Declared> {
    let leaf = prop_oneof![
        prop::sample::select(vec![
            "int",
            "float",
            "bool",
            "string",
            "color",
            "Bar",
            "chart.point"
        ])
        .prop_map(|name| element(name, 0)),
        prop::sample::select(vec!["int[]", "Bar[]", "color[]", "chart.point[]"])
            .prop_map(|name| element(name, 1)),
    ];
    leaf.prop_recursive(6, 48, 2, |inner| {
        prop_oneof![
            (
                inner.clone(),
                prop::sample::select(NESTINGS.to_vec()),
                1usize..200
            )
                .prop_map(|(element, (open, close), count)| Declared {
                    text: nested(open, &element.text, close, count),
                    lists: element.lists + count,
                    levels: element.levels + count,
                }),
            (inner.clone(), inner).prop_map(|(key, value)| Declared {
                text: format!("map<{}, {}>", key.text, value.text),
                lists: 1 + key.lists.max(value.lists),
                levels: 1 + key.levels.max(value.levels),
            }),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// A type parses exactly when its argument lists nest within the cap, in
    /// every position; a refused one costs one diagnostic and nothing else.
    #[test]
    fn a_type_parses_exactly_when_it_is_within_the_cap(
        declared in declared_type(),
        position in prop::sample::select(POSITIONS.to_vec()),
    ) {
        let result = parse(&script(position, &declared.text));
        if declared.lists <= CAP {
            prop_assert!(
                !has_errors(&result.diagnostics),
                "{} lists: {:?}",
                declared.lists,
                result.diagnostics.first()
            );
            prop_assert_eq!(declared_levels(position, &result.program), Some(declared.levels));
        } else {
            prop_assert_eq!(too_deep(&result.diagnostics), 1, "{} lists", declared.lists);
            prop_assert_eq!(result.diagnostics.len(), 1, "{:?}", result.diagnostics);
            prop_assert_eq!(declared_levels(position, &result.program), None);
        }
        prop_assert!(parsed_the_statement_after(&result.program));
    }
}
