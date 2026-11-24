//! Compile-time provenance for automatic style color selectors.
//! Series selection between known choices is distinct from calculating a
//! runtime color. No runtime VM values participate in this classification.
use crate::*;
use pinewasm_hir::{StyleColorPaletteEntry, StyleColorSource};

type Origins<'a> = HashMap<SymbolId, Vec<&'a HirExpr>>;

#[derive(Clone, Copy)]
enum Family {
    Plot,
    Visual,
    Fill,
}
type Output<'a> = (Family, u32, &'a [(PlotColorRole, HirExpr)]);

fn collect<'a>(body: &'a [HirStmt], origins: &mut Origins<'a>, outputs: &mut Vec<Output<'a>>) {
    for statement in body {
        match statement {
            HirStmt::Assign { symbol, value, .. } if value.ty == Type::Color => {
                origins.entry(*symbol).or_default().push(value);
            }
            HirStmt::Destructure { targets, value, .. } => {
                if let HirExprKind::Tuple { elements } = &value.kind {
                    for (symbol, expr) in targets.iter().zip(elements) {
                        if expr.ty == Type::Color {
                            origins.entry(*symbol).or_default().push(expr);
                        }
                    }
                }
            }
            HirStmt::Plot { slot, colors, .. } => outputs.push((Family::Plot, slot.0, colors)),
            HirStmt::Visual { slot, colors, .. } => outputs.push((Family::Visual, slot.0, colors)),
            HirStmt::Fill { slot, colors, .. } => outputs.push((Family::Fill, slot.0, colors)),
            HirStmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                collect(then_branch, origins, outputs);
                collect(else_branch, origins, outputs);
            }
            HirStmt::For { body, .. } | HirStmt::While { body, .. } => {
                collect(body, origins, outputs);
            }
            _ => {}
        }
    }
}

fn append_unique(destination: &mut Vec<StyleColorSource>, sources: Vec<StyleColorSource>) {
    for source in sources {
        if !destination.contains(&source) {
            destination.push(source);
        }
    }
}

impl Analyzer<'_> {
    fn automatic_color_sources(
        &self,
        expr: &HirExpr,
        origins: &Origins<'_>,
        visiting: &mut HashSet<SymbolId>,
    ) -> Option<Vec<StyleColorSource>> {
        let constant = |value: InputValue| match value {
            InputValue::Color { r, g, b, t } => {
                Some(vec![StyleColorSource::Constant { r, g, b, t }])
            }
            _ => None,
        };
        match &expr.kind {
            HirExprKind::ConstColor { r, g, b, t } => Some(vec![StyleColorSource::Constant {
                r: *r,
                g: *g,
                b: *b,
                t: *t,
            }]),
            HirExprKind::ConstNa => Some(Vec::new()),
            HirExprKind::ReadInput { input } => self
                .inputs
                .get(*input as usize)
                .filter(|decl| decl.kind == InputKind::Color)
                .map(|_| vec![StyleColorSource::Input { input: *input }]),
            HirExprKind::ReadVar { symbol, .. } | HirExprKind::ReadVarDynamic { symbol, .. } => {
                // A persistent color may retain a previous choice through a
                // self-read. Its initializer and other writes still contribute.
                if !visiting.insert(*symbol) {
                    return Some(Vec::new());
                }
                let result = origins.get(symbol).and_then(|expressions| {
                    let mut result = Vec::new();
                    for expression in expressions {
                        append_unique(
                            &mut result,
                            self.automatic_color_sources(expression, origins, visiting)?,
                        );
                    }
                    Some(result)
                });
                visiting.remove(symbol);
                result
            }
            HirExprKind::Ternary { then_e, else_e, .. } => {
                let mut colors = self.automatic_color_sources(then_e, origins, visiting)?;
                append_unique(
                    &mut colors,
                    self.automatic_color_sources(else_e, origins, visiting)?,
                );
                Some(colors)
            }
            HirExprKind::MaterializeHistory { expr, .. }
            | HirExprKind::MaterializeHistoryDynamic { expr, .. } => {
                self.automatic_color_sources(expr, origins, visiting)
            }
            HirExprKind::CallPure {
                func: PureFunc::CastColor,
                args,
            } => self.automatic_color_sources(args.first()?, origins, visiting),
            _ => self.const_input_value_from_hir(expr).and_then(constant),
        }
    }

    pub(crate) fn finalize_automatic_colors(&mut self, body: &[HirStmt]) {
        let mut origins = HashMap::new();
        let mut outputs = Vec::new();
        collect(body, &mut origins, &mut outputs);
        let mut palettes = Vec::new();
        let mut all_known = true;
        for (family, slot, colors) in outputs {
            let mut palette = Vec::new();
            let mut known = true;
            for (role, expr) in colors {
                if expr.ty != Type::Color {
                    continue;
                }
                match self.automatic_color_sources(expr, &origins, &mut HashSet::new()) {
                    Some(sources) => {
                        palette.extend(sources.into_iter().map(|source| StyleColorPaletteEntry {
                            role: *role,
                            source,
                            resolved_color: None,
                        }));
                    }
                    None => known = false,
                }
            }
            all_known &= known;
            let has_color = colors.iter().any(|(_, expr)| expr.ty == Type::Color);
            palettes.push((family, slot, palette, has_color));
        }
        let editable = !self.dynamic_color_calculation && all_known;
        for (family, slot, palette, has_color) in palettes {
            let style = match family {
                Family::Plot => &mut self.plots[slot as usize].style,
                Family::Visual => &mut self.visuals[slot as usize].style,
                Family::Fill => &mut self.fills[slot as usize].style,
            };
            style.color_editable = editable && (!has_color || !palette.is_empty());
            style.color_palette = palette;
        }
    }
}
