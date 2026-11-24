//! Hierarchical program outline from the AST: the script's actual shape
//! (declaration, inputs grouped by their Pine `group=` strings, functions
//! with locals, types, the plot/alert/strategy output surface)
//! rather than a flat symbol list. Transport-free; the wasm module exports it
//! for editor outline views.

use pinewasm_parse::parse;
use pinewasm_syntax::{CallArg, EnumDecl, Expr, FuncDef, Item, Stmt, TypeDecl};
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutlineNode {
    pub label: String,
    /// "declaration" | "import" | "group" | "input" | "variable" | "function"
    /// | "local" | "type" | "field" | "enum" | "member"
    /// | "plot" | "alert" | "strategy"
    pub kind: &'static str,
    /// Secondary text: an input's human title and flavor, a var/varip chip,
    /// the callee a variable is computed from, a param's declared type.
    pub detail: Option<String>,
    /// Byte offset of the node's declaration (the wasm layer maps to a line).
    pub span_start: u32,
    pub children: Vec<OutlineNode>,
}

fn node(label: impl Into<String>, kind: &'static str, span_start: u32) -> OutlineNode {
    OutlineNode {
        label: label.into(),
        kind,
        detail: None,
        span_start,
        children: Vec::new(),
    }
}

/// Unwrap parenthesized groups to the underlying expression.
fn unwrap(expr: &Expr) -> &Expr {
    match expr {
        Expr::Group { inner, .. } => unwrap(inner),
        other => other,
    }
}

/// The first positional string literal, skipping `skip` leading positionals —
/// how `input.int(defval, "Title")` and `indicator("Title")` carry titles.
fn positional_str(args: &[CallArg], skip: usize) -> Option<&str> {
    args.iter()
        .filter(|arg| arg.name.is_none())
        .skip(skip)
        .find_map(|arg| match unwrap(&arg.value) {
            Expr::Str { value, .. } => Some(value.as_str()),
            _ => None,
        })
}

/// A named argument's string value, resolving bare identifiers through the
/// script's string constants (`grpMA = "Moving averages"` … `group = grpMA`).
fn named_str<'a>(
    args: &'a [CallArg],
    name: &str,
    consts: &'a HashMap<String, String>,
) -> Option<String> {
    let arg = args
        .iter()
        .find(|arg| arg.name.as_ref().is_some_and(|ident| ident.name == name))?;
    match unwrap(&arg.value) {
        Expr::Str { value, .. } => Some(value.clone()),
        Expr::Ident(ident) => consts.get(ident.name.as_str()).cloned(),
        _ => None,
    }
}

/// Output-surface classification for a bare call statement.
fn call_bucket(callee: &str) -> Option<(&'static str, &'static str)> {
    const PLOTS: [&str; 8] = [
        "plot",
        "plotshape",
        "plotchar",
        "plotcandle",
        "plotbar",
        "plotarrow",
        "hline",
        "fill",
    ];
    if PLOTS.contains(&callee) || callee == "bgcolor" || callee == "barcolor" {
        return Some(("plots", "plot"));
    }
    if callee == "alertcondition" || callee == "alert" {
        return Some(("alerts", "alert"));
    }
    if callee.starts_with("strategy.") {
        return Some(("strategy", "strategy"));
    }
    None
}

fn variable_node(
    name: &str,
    value: &Expr,
    is_var: bool,
    is_varip: bool,
    span_start: u32,
) -> OutlineNode {
    let mut out = node(name, "variable", span_start);
    let mut detail = Vec::new();
    if is_varip {
        detail.push("varip".to_string());
    } else if is_var {
        detail.push("var".to_string());
    }
    if let Expr::Call { callee, .. } = unwrap(value) {
        detail.push(format!("{}(…)", callee.name));
    }
    if !detail.is_empty() {
        out.detail = Some(detail.join(" · "));
    }
    out
}

fn func_node(def: &FuncDef) -> OutlineNode {
    let mut out = node(&def.name.name, "function", def.span.start);
    let params = def
        .params
        .iter()
        .map(|param| param.name.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let mut chips = format!("({params})");
    if def.exported {
        chips.push_str(" · export");
    }
    if def.receiver.is_some() {
        chips.push_str(" · method");
    }
    out.detail = Some(chips);
    for stmt in &def.body {
        if let Stmt::Assign { name, span, .. } = stmt {
            out.children.push(node(&name.name, "local", span.start));
        }
    }
    out
}

fn type_node(decl: &TypeDecl) -> OutlineNode {
    let mut out = node(&decl.name.name, "type", decl.span.start);
    if decl.exported {
        out.detail = Some("export".to_string());
    }
    for field in &decl.fields {
        out.children
            .push(node(&field.name.name, "field", field.span.start));
    }
    out
}

fn enum_node(decl: &EnumDecl) -> OutlineNode {
    let mut out = node(&decl.name.name, "enum", decl.span.start);
    for variant in &decl.variants {
        out.children
            .push(node(&variant.name.name, "member", variant.span.start));
    }
    out
}

/// Build the hierarchical outline. Parse-tolerant: whatever the parse tree
/// carries renders, compile errors or not.
pub fn outline_from_source(source: &str) -> Vec<OutlineNode> {
    let program = parse(source).program;

    // Pass 1: string constants, so `group = grpMA` resolves to its title.
    let mut consts: HashMap<String, String> = HashMap::new();
    for item in &program.items {
        if let Item::Stmt(Stmt::Assign { name, value, .. }) = item {
            if let Expr::Str { value: text, .. } = unwrap(value) {
                consts.insert(name.name.clone(), text.clone());
            }
        }
    }

    let mut declaration: Option<OutlineNode> = None;
    let mut imports: Vec<OutlineNode> = Vec::new();
    // Input groups keep first-appearance order. `group_positions` finds a
    // group's bucket by title, so a script whose inputs each name their own
    // group still costs one lookup per input, not one scan of every group.
    let mut input_groups: Vec<(String, Vec<OutlineNode>)> = Vec::new();
    let mut group_positions: HashMap<String, usize> = HashMap::new();
    let mut variables: Vec<OutlineNode> = Vec::new();
    let mut functions: Vec<OutlineNode> = Vec::new();
    let mut types: Vec<OutlineNode> = Vec::new();
    let mut plots: Vec<OutlineNode> = Vec::new();
    let mut alerts: Vec<OutlineNode> = Vec::new();
    let mut strategy: Vec<OutlineNode> = Vec::new();

    let mut push_input = |group: Option<String>, input: OutlineNode| {
        let key = group.unwrap_or_default();
        if let Some(&position) = group_positions.get(&key) {
            input_groups[position].1.push(input);
        } else {
            group_positions.insert(key.clone(), input_groups.len());
            input_groups.push((key, vec![input]));
        }
    };

    for item in &program.items {
        match item {
            Item::Indicator(decl) => {
                let title = positional_str(&decl.args, 0).unwrap_or("indicator");
                let mut head = node(title, "declaration", decl.span.start);
                head.detail = Some("indicator".to_string());
                declaration = Some(head);
            }
            Item::Strategy(decl) => {
                let title = positional_str(&decl.args, 0).unwrap_or("strategy");
                let mut head = node(title, "declaration", decl.span.start);
                head.detail = Some("strategy".to_string());
                declaration = Some(head);
            }
            Item::Library(decl) => {
                let title = positional_str(&decl.args, 0).unwrap_or("library");
                let mut head = node(title, "declaration", decl.span.start);
                head.detail = Some("library".to_string());
                declaration = Some(head);
            }
            Item::Import(decl) => {
                let mut import = node(&decl.alias.name, "import", decl.span.start);
                import.detail = Some(format!(
                    "{}/{}/{}",
                    decl.publisher.name, decl.library.name, decl.version
                ));
                imports.push(import);
            }
            Item::FuncDef(def) => functions.push(func_node(def)),
            Item::TypeDef(decl) => types.push(type_node(decl)),
            Item::EnumDef(decl) => types.push(enum_node(decl)),
            Item::Stmt(Stmt::Assign {
                name,
                value,
                is_var,
                is_varip,
                span,
                ..
            }) => match unwrap(value) {
                Expr::Call { callee, args, .. } if callee.name.starts_with("input") => {
                    let mut input = node(&name.name, "input", span.start);
                    let flavor = callee.name.strip_prefix("input.").unwrap_or("source");
                    let title = named_str(args, "title", &consts)
                        .or_else(|| positional_str(args, 1).map(str::to_string));
                    input.detail = Some(match title {
                        Some(title) => format!("{title} · {flavor}"),
                        None => flavor.to_string(),
                    });
                    push_input(named_str(args, "group", &consts), input);
                }
                _ => variables.push(variable_node(
                    &name.name, value, *is_var, *is_varip, span.start,
                )),
            },
            Item::Stmt(Stmt::Destructure {
                targets,
                value,
                span,
            }) => {
                for target in targets {
                    variables.push(variable_node(&target.name, value, false, false, span.start));
                }
            }
            Item::Stmt(Stmt::Expr { expr, span }) => {
                if let Expr::Call { callee, args, .. } = unwrap(expr) {
                    if let Some((bucket, kind)) = call_bucket(&callee.name) {
                        let mut call = node(&callee.name, kind, span.start);
                        call.detail = positional_str(args, 0)
                            .map(str::to_string)
                            .or_else(|| named_str(args, "title", &consts));
                        // Show the title as the label when one exists.
                        if let Some(title) = call.detail.take() {
                            call.detail = Some(call.label.clone());
                            call.label = title;
                        }
                        match bucket {
                            "plots" => plots.push(call),
                            "alerts" => alerts.push(call),
                            _ => strategy.push(call),
                        }
                    }
                }
            }
            Item::Stmt(_) => {}
        }
    }

    // Assemble: declaration first, then non-empty sections in reading order.
    let mut out = Vec::new();
    if let Some(head) = declaration {
        out.push(head);
    }
    let section = |label: &str, children: Vec<OutlineNode>| -> Option<OutlineNode> {
        if children.is_empty() {
            return None;
        }
        let start = children.first().map_or(0, |child| child.span_start);
        let mut group = node(label, "group", start);
        group.children = children;
        Some(group)
    };
    out.extend(section("Imports", imports));
    if !input_groups.is_empty() {
        let start = input_groups[0].1.first().map_or(0, |n| n.span_start);
        let mut inputs = node("Inputs", "group", start);
        for (name, bucket) in input_groups {
            if name.is_empty() {
                inputs.children.extend(bucket);
            } else {
                let start = bucket.first().map_or(0, |n| n.span_start);
                let mut sub = node(name, "group", start);
                sub.children = bucket;
                inputs.children.push(sub);
            }
        }
        out.push(inputs);
    }
    out.extend(section("Variables", variables));
    out.extend(section("Functions", functions));
    out.extend(section("Types", types));
    out.extend(section("Plots & Visuals", plots));
    out.extend(section("Alerts", alerts));
    out.extend(section("Strategy", strategy));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_inputs_by_resolved_group_titles() {
        let source = "//@version=6\nindicator(\"Demo\")\ngrp = \"Tuning\"\nlen = input.int(14, \"Length\", group = grp)\nsrc = input.source(close, \"Source\")\nf(x) => x * 2\nplot(f(len), \"Doubled\")\n";
        let outline = outline_from_source(source);
        assert_eq!(outline[0].kind, "declaration");
        assert_eq!(outline[0].label, "Demo");
        let inputs = outline.iter().find(|n| n.label == "Inputs").unwrap();
        let tuning = inputs
            .children
            .iter()
            .find(|n| n.label == "Tuning")
            .unwrap();
        assert_eq!(tuning.children[0].label, "len");
        assert_eq!(tuning.children[0].detail.as_deref(), Some("Length · int"));
        assert!(inputs.children.iter().any(|n| n.label == "src"));
        let plots = outline
            .iter()
            .find(|n| n.label == "Plots & Visuals")
            .unwrap();
        assert_eq!(plots.children[0].label, "Doubled");
        let funcs = outline.iter().find(|n| n.label == "Functions").unwrap();
        assert_eq!(funcs.children[0].detail.as_deref(), Some("(x)"));
        assert!(funcs.children[0].children.is_empty());
    }

    fn labels(nodes: &[OutlineNode]) -> Vec<&str> {
        nodes.iter().map(|node| node.label.as_str()).collect()
    }

    #[test]
    fn input_groups_keep_first_appearance_order() {
        // A group sits where its first input appeared and collects the later
        // ones; ungrouped inputs stay at the top of the section, where the
        // first of them appeared.
        let source = "indicator(\"Demo\")\n\
            a = input.int(1, \"A\", group = \"Trend\")\n\
            b = input.int(2, \"B\")\n\
            c = input.float(3.0, \"C\", group = \"Risk\")\n\
            d = input.bool(true, \"D\", group = \"Trend\")\n\
            e = input.int(5, \"E\")\n\
            f = input.int(6, \"F\", group = \"Risk\")\n";
        let outline = outline_from_source(source);
        let inputs = outline.iter().find(|n| n.label == "Inputs").unwrap();
        assert_eq!(labels(&inputs.children), ["Trend", "b", "e", "Risk"]);
        assert_eq!(labels(&inputs.children[0].children), ["a", "d"]);
        assert_eq!(labels(&inputs.children[3].children), ["c", "f"]);
        assert_eq!(inputs.children[0].kind, "group");
        assert_eq!(inputs.children[1].kind, "input");
    }

    #[test]
    fn every_input_may_name_its_own_group() {
        // One group per input is the widest the section gets: 25,000 of them
        // fill most of the largest source the outline accepts.
        const GROUPS: usize = 25_000;
        let mut source = String::from("indicator(\"Wide\")\n");
        for index in 0..GROUPS {
            source.push_str(&format!("x{index} = input.int(0, group = \"g{index}\")\n"));
        }
        // A second input for the first and the last group, far from the first.
        source.push_str("first = input.int(0, group = \"g0\")\n");
        source.push_str(&format!(
            "last = input.int(0, group = \"g{}\")\n",
            GROUPS - 1
        ));

        let outline = outline_from_source(&source);
        let inputs = outline.iter().find(|n| n.label == "Inputs").unwrap();
        assert_eq!(inputs.children.len(), GROUPS);
        for (index, group) in inputs.children.iter().enumerate() {
            assert_eq!(group.label, format!("g{index}"));
            assert_eq!(group.children[0].label, format!("x{index}"));
        }
        assert_eq!(labels(&inputs.children[0].children), ["x0", "first"]);
        assert_eq!(
            labels(&inputs.children[GROUPS - 1].children),
            [format!("x{}", GROUPS - 1).as_str(), "last"]
        );
        let grouped: usize = inputs.children.iter().map(|g| g.children.len()).sum();
        assert_eq!(grouped, GROUPS + 2);
    }
}
