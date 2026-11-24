//! Deferred proof for persistent reads that the lexical summary cannot settle.
//!
//! This is metadata analysis over the HIR that normal semantic analysis already
//! produced. It neither expands a call nor runs a Pine expression. In particular,
//! the existing typed constant folder is the only arithmetic evaluator used to
//! discharge a dead branch. Input defaults and carried initializers are never
//! facts in that proof.
use crate::*;

type Roots = HashSet<SymbolId>;

#[derive(Default)]
pub(super) struct TemporalFixedness {
    roots: HashMap<SymbolId, Span>,
    /// The binding at the actual analysis occurrence, before a later `:=`.
    current: HashMap<SymbolId, Roots>,
    /// Sparse overlays for branch/loop observations; only changed symbols live here.
    current_layers: Vec<HashMap<SymbolId, Roots>>,
    /// A monotone union is used only to strengthen executable data qualifiers.
    /// Fixed uses below retain their own occurrence snapshot instead.
    all: HashMap<SymbolId, Roots>,
    uses: Vec<FixedUse>,
    /// Source-closed values established by unconditional assignments before a use.
    /// Persistent initializers and input defaults never populate this map.
    source_closed_values: HashMap<SymbolId, InputValue>,
    /// Match a proven write to its HIR statement; branch observations invalidate it.
    source_closed_reset_spans: HashMap<SymbolId, Span>,
    trusted_strengthenings: HashSet<(SymbolId, Span)>,
}

struct FixedUse {
    roots: Roots,
    span: Span,
    call: String,
    parameter: String,
    /// This exact source occurrence closes after an unconditional reset.
    source_closed_value: Option<InputValue>,
}

impl TemporalFixedness {
    fn dependencies(&self, expr: &HirExpr) -> Roots {
        if self.roots.is_empty() {
            return Roots::new();
        }
        fn collect(
            expr: &HirExpr,
            roots: &HashMap<SymbolId, Span>,
            bindings: &HashMap<SymbolId, Roots>,
            current_layers: &[HashMap<SymbolId, Roots>],
            overlays: &mut Vec<HashMap<SymbolId, Roots>>,
        ) -> Roots {
            if let HirExprKind::EvalSequence {
                bindings: captures,
                result,
            } = &expr.kind
            {
                overlays.push(HashMap::with_capacity(captures.len()));
                for (slot, value) in captures {
                    let dependencies = collect(value, roots, bindings, current_layers, overlays);
                    overlays
                        .last_mut()
                        .expect("EvalSequence binding scope was pushed")
                        .insert(SymbolId(slot.0), dependencies);
                }
                let dependencies = collect(result, roots, bindings, current_layers, overlays);
                overlays.pop();
                return dependencies;
            }

            let mut result = Roots::new();
            if let HirExprKind::ReadVar { symbol, .. }
            | HirExprKind::ReadVarDynamic { symbol, .. } = expr.kind
            {
                if roots.contains_key(&symbol) {
                    result.insert(symbol);
                }
                if let Some(inherited) = overlays
                    .iter()
                    .rev()
                    .find_map(|scope| scope.get(&symbol))
                    .or_else(|| {
                        current_layers
                            .iter()
                            .rev()
                            .find_map(|scope| scope.get(&symbol))
                    })
                    .or_else(|| bindings.get(&symbol))
                {
                    result.extend(inherited);
                }
            }
            expr_children(expr, &mut |child| {
                result.extend(collect(child, roots, bindings, current_layers, overlays));
            });
            result
        }
        collect(
            expr,
            &self.roots,
            &self.current,
            &self.current_layers,
            &mut Vec::new(),
        )
    }

    fn extend_current(&mut self, symbol: SymbolId, roots: &Roots) {
        if let Some(layer) = self.current_layers.last_mut() {
            layer
                .entry(symbol)
                .or_default()
                .extend(roots.iter().copied());
        } else {
            self.current
                .entry(symbol)
                .or_default()
                .extend(roots.iter().copied());
        }
    }

    fn binding(&mut self, symbol: SymbolId, expr: &HirExpr) {
        if self.roots.is_empty() {
            return;
        }
        let mut roots = self.dependencies(expr);
        if self.roots.contains_key(&symbol) {
            roots.insert(symbol);
        }
        self.all.entry(symbol).or_default().extend(&roots);
        self.extend_current(symbol, &roots);
    }

    fn join_current(&mut self, other: HashMap<SymbolId, Roots>) {
        for (symbol, roots) in other {
            self.extend_current(symbol, &roots);
        }
    }

    fn observe_in_layer(
        &mut self,
        analyzer: &Analyzer<'_>,
        body: &[HirStmt],
        controls: &Roots,
        conditional: bool,
    ) -> HashMap<SymbolId, Roots> {
        self.current_layers.push(HashMap::new());
        self.observe_controlled(analyzer, body, controls, conditional);
        self.current_layers
            .pop()
            .expect("observed control region pushed a binding layer")
    }

    fn observe(&mut self, analyzer: &Analyzer<'_>, body: &[HirStmt]) {
        self.observe_controlled(analyzer, body, &Roots::new(), false);
    }

    fn observe_controlled(
        &mut self,
        analyzer: &Analyzer<'_>,
        body: &[HirStmt],
        controls: &Roots,
        conditional: bool,
    ) {
        for statement in body {
            match statement {
                HirStmt::Assign { symbol, value, .. } => {
                    let proven_reset = self
                        .source_closed_reset_spans
                        .remove(symbol)
                        .is_some_and(|span| span == value.span);
                    if conditional || !proven_reset {
                        self.source_closed_values.remove(symbol);
                    }
                    self.binding(*symbol, value);
                    self.extend_current(*symbol, controls);
                    self.all.entry(*symbol).or_default().extend(controls);
                }
                HirStmt::Destructure { targets, value, .. } => {
                    if let HirExprKind::Tuple { elements } = &value.kind {
                        for (target, element) in targets.iter().zip(elements) {
                            self.source_closed_values.remove(target);
                            self.source_closed_reset_spans.remove(target);
                            self.binding(*target, element);
                            self.extend_current(*target, controls);
                            self.all.entry(*target).or_default().extend(controls);
                        }
                    }
                }
                HirStmt::If {
                    cond,
                    then_branch,
                    else_branch,
                    ..
                } => {
                    let mut branch_controls = controls.clone();
                    branch_controls.extend(self.dependencies(cond));
                    match source_closed_temporal_value(analyzer, cond, &self.source_closed_values) {
                        Some(InputValue::Bool(true)) => {
                            self.observe_controlled(analyzer, then_branch, &branch_controls, true);
                        }
                        Some(InputValue::Bool(false)) => {
                            self.observe_controlled(analyzer, else_branch, &branch_controls, true);
                        }
                        _ => {
                            let then_values = self.observe_in_layer(
                                analyzer,
                                then_branch,
                                &branch_controls,
                                true,
                            );
                            let else_values = self.observe_in_layer(
                                analyzer,
                                else_branch,
                                &branch_controls,
                                true,
                            );
                            self.join_current(then_values);
                            self.join_current(else_values);
                        }
                    }
                }
                HirStmt::For {
                    start,
                    end,
                    step,
                    body,
                    ..
                } => {
                    let mut loop_controls = controls.clone();
                    for value in [start, end, step] {
                        loop_controls.extend(self.dependencies(value));
                    }
                    let values = self.observe_in_layer(analyzer, body, &loop_controls, true);
                    self.join_current(values);
                }
                HirStmt::While { cond, body, .. } => {
                    let mut loop_controls = controls.clone();
                    loop_controls.extend(self.dependencies(cond));
                    if source_closed_temporal_value(analyzer, cond, &self.source_closed_values)
                        != Some(InputValue::Bool(false))
                    {
                        let values = self.observe_in_layer(analyzer, body, &loop_controls, true);
                        self.join_current(values);
                    }
                }
                _ => {}
            }
        }
    }
}

impl Analyzer<'_> {
    pub(crate) fn temporal_dependencies(&self, expr: &HirExpr) -> bool {
        !self
            .temporal_fixedness
            .borrow()
            .dependencies(expr)
            .is_empty()
    }

    /// Save the original dependency before a fixed serializer, selector or
    /// run-once capture replaces it with its initializer-shaped value.
    pub(crate) fn defer_temporal_fixedness_at(
        &self,
        expr: &HirExpr,
        call: &str,
        parameter: &str,
        span: Span,
    ) {
        if expr.qualifier > Qualifier::Simple {
            return;
        }
        let mut book = self.temporal_fixedness.borrow_mut();
        let roots = book.dependencies(expr);
        if roots.is_empty() {
            return;
        }
        let source_closed_value =
            source_closed_temporal_value(self, expr, &book.source_closed_values);
        book.uses.push(FixedUse {
            roots,
            span,
            call: call.to_string(),
            parameter: parameter.to_string(),
            source_closed_value,
        });
    }

    pub(crate) fn temporal_binding(&self, symbol: SymbolId, value: &HirExpr) {
        let user_symbol = self
            .symbols
            .get(symbol.0 as usize)
            .is_some_and(|declaration| !declaration.synthetic);
        let can_reset_each_execution =
            user_symbol && self.assignment_controls.is_empty() && self.loop_depth == 0;
        let source_closed_value = if can_reset_each_execution {
            let book = self.temporal_fixedness.borrow();
            source_closed_temporal_value(self, value, &book.source_closed_values)
        } else {
            None
        };
        let mut book = self.temporal_fixedness.borrow_mut();
        book.source_closed_reset_spans.remove(&symbol);
        if let Some(source_closed_value) = source_closed_value {
            book.source_closed_values
                .insert(symbol, source_closed_value);
            book.source_closed_reset_spans.insert(symbol, value.span);
        } else {
            book.source_closed_values.remove(&symbol);
        }
        if self.source_closed_active_origins.contains(&symbol)
            && book.dependencies(value).is_empty()
        {
            book.trusted_strengthenings.insert((symbol, value.span));
        }
        book.binding(symbol, value);
    }

    pub(crate) fn temporal_reassignment(&self, symbol: SymbolId, value: &HirExpr) {
        let user_symbol = self
            .symbols
            .get(symbol.0 as usize)
            .is_some_and(|declaration| !declaration.synthetic);
        let can_reset_each_execution =
            user_symbol && self.assignment_controls.is_empty() && self.loop_depth == 0;
        let source_closed_value = if can_reset_each_execution {
            let book = self.temporal_fixedness.borrow();
            source_closed_temporal_value(self, value, &book.source_closed_values)
        } else {
            None
        };
        let mut book = self.temporal_fixedness.borrow_mut();
        book.source_closed_reset_spans.remove(&symbol);
        if let Some(source_closed_value) = source_closed_value {
            book.source_closed_values
                .insert(symbol, source_closed_value);
            book.source_closed_reset_spans.insert(symbol, value.span);
        } else {
            book.source_closed_values.remove(&symbol);
        }
        // Only a source-unconditional assignment replaces the previous alias
        // value. Conditional and loop alternatives preserve a conservative
        // union, including the branch/zero-trip path that does not assign.
        if can_reset_each_execution && !book.roots.contains_key(&symbol) {
            book.current.remove(&symbol);
        }
        book.binding(symbol, value);
    }

    pub(crate) fn observe_temporal_pending(&self, body: &[HirStmt]) {
        let mut book = self.temporal_fixedness.borrow_mut();
        if !book.roots.is_empty() {
            book.observe(self, body);
        }
    }

    pub(crate) fn temporal_declaration(&self, symbol: SymbolId, value: &HirExpr) {
        let declaration = &self.symbols[symbol.0 as usize];
        let is_temporal_root = declaration.qualifier < Qualifier::Series
            && self
                .persistent_write_plan
                .potential_carried_reads
                .contains(&declaration.decl_span)
            && matches!(
                declaration.ty,
                Type::Int
                    | Type::Float
                    | Type::Bool
                    | Type::String
                    | Type::Color
                    | Type::Enum(_)
                    | Type::TextFormat
                    | Type::PlotDisplay
                    | Type::PlotSimpleDisplay
            );
        let can_reset_each_execution = !is_temporal_root
            && !declaration.persistent
            && !declaration.varip
            && !declaration.synthetic
            && self.assignment_controls.is_empty()
            && self.loop_depth == 0;
        let source_closed_value = if can_reset_each_execution {
            let book = self.temporal_fixedness.borrow();
            source_closed_temporal_value(self, value, &book.source_closed_values)
        } else {
            None
        };
        let mut book = self.temporal_fixedness.borrow_mut();
        if is_temporal_root {
            book.roots.insert(symbol, declaration.decl_span);
        }
        book.source_closed_reset_spans.remove(&symbol);
        if let Some(source_closed_value) = source_closed_value {
            book.source_closed_values
                .insert(symbol, source_closed_value);
            book.source_closed_reset_spans.insert(symbol, value.span);
        } else {
            book.source_closed_values.remove(&symbol);
        }
        if is_temporal_root {
            // A persistent initializer is not a reset that runs each bar.
            book.source_closed_values.remove(&symbol);
            book.source_closed_reset_spans.remove(&symbol);
        }
        if self.source_closed_active_origins.contains(&symbol)
            && book.dependencies(value).is_empty()
        {
            book.trusted_strengthenings.insert((symbol, value.span));
        }
        book.binding(symbol, value);
    }

    pub(crate) fn finalize_temporal_fixedness(&mut self, body: &mut [HirStmt]) {
        let book = self.temporal_fixedness.take();
        if book.roots.is_empty() {
            return;
        }
        let mut facts = Facts::default();
        self.temporal_writes(body, &book.roots, &book.trusted_strengthenings, &mut facts);
        if facts.writes.is_empty() {
            return;
        }
        let unsafe_roots: Roots = facts.writes.keys().copied().collect();
        let mut reported = HashSet::new();
        for usage in book.uses {
            let has_source_closed_value = usage.source_closed_value.is_some();
            let Some(root) = usage
                .roots
                .iter()
                .filter(|root| unsafe_roots.contains(root))
                .min_by_key(|root| root.0)
            else {
                continue;
            };
            if !reported.insert((usage.span, usage.call.clone(), usage.parameter.clone())) {
                continue;
            }
            self.diagnostics.push(
                Diagnostic::error(
                    codes::E0252,
                    format!("cannot prove that this persistent value is fixed for `{}` argument `{}`", usage.call, usage.parameter),
                )
                .with_label(usage.span, "this compiler needs a proved fixed value here")
                .with_label(facts.writes[root], "this write may affect the persistent value on another execution")
                .with_note(if has_source_closed_value {
                    "the value closes at this use, but this compiler has not verified a renderer-safe style encoding"
                } else {
                    "this is a local compiler limitation, not a claim that the Pine source is invalid"
                }),
            );
        }
        let promoted: Roots = book
            .all
            .iter()
            .filter(|(_, roots)| !roots.is_disjoint(&unsafe_roots))
            .map(|(symbol, _)| *symbol)
            .chain(unsafe_roots.iter().copied())
            .collect();
        for symbol in &promoted {
            if let Some(symbol) = self.symbols.get_mut(symbol.0 as usize) {
                symbol.qualifier = Qualifier::Series;
                symbol.const_int = None;
            }
        }
        let static_requests = !self.dynamic_requests_enabled();
        self.requalify_temporal_body(body, &promoted, static_requests, false, &mut reported);
    }

    /// Walk already-inlined statements. Values are recorded only when the
    /// existing typed folder proves them from source-only, noncarried facts.
    fn temporal_writes(
        &self,
        body: &[HirStmt],
        roots: &HashMap<SymbolId, Span>,
        trusted: &HashSet<(SymbolId, Span)>,
        facts: &mut Facts,
    ) {
        for statement in body {
            match statement {
                HirStmt::Assign {
                    symbol,
                    value,
                    init,
                    span,
                } => {
                    if roots.contains_key(symbol) {
                        facts.remove_value(*symbol);
                        if !init {
                            facts.record_write(*symbol, *span);
                        }
                    } else if let Some(value) = self.temporal_source_value(
                        value,
                        &facts.values,
                        trusted.contains(&(*symbol, value.span)),
                    ) {
                        facts.set_value(*symbol, value);
                    } else {
                        facts.remove_value(*symbol);
                    }
                }
                HirStmt::Destructure { targets, value, .. } => {
                    if let HirExprKind::Tuple { elements } = &value.kind {
                        for (target, value) in targets.iter().zip(elements) {
                            if let Some(value) = self.temporal_source_value(
                                value,
                                &facts.values,
                                trusted.contains(&(*target, value.span)),
                            ) {
                                facts.set_value(*target, value);
                            } else {
                                facts.remove_value(*target);
                            }
                        }
                    } else {
                        for target in targets {
                            facts.remove_value(*target);
                        }
                    }
                }
                HirStmt::If {
                    cond,
                    then_branch,
                    else_branch,
                    ..
                } => match self.temporal_source_value(cond, &facts.values, false) {
                    Some(InputValue::Bool(true)) => {
                        self.temporal_writes(then_branch, roots, trusted, facts);
                    }
                    Some(InputValue::Bool(false)) => {
                        self.temporal_writes(else_branch, roots, trusted, facts);
                    }
                    _ => {
                        let value_mark = facts.value_mark();
                        let write_mark = facts.write_mark();
                        self.temporal_writes(then_branch, roots, trusted, facts);
                        let then_values = facts.changed_values_since(value_mark);
                        let then_writes = facts.changed_writes_since(write_mark);
                        facts.rollback_values_to(value_mark);
                        facts.rollback_writes_to(write_mark);
                        self.temporal_writes(else_branch, roots, trusted, facts);
                        let else_values = facts.changed_values_since(value_mark);
                        let else_writes = facts.changed_writes_since(write_mark);
                        facts.rollback_values_to(value_mark);
                        facts.rollback_writes_to(write_mark);
                        facts.merge_branch_values(then_values, else_values);
                        facts.merge_branch_writes(then_writes, else_writes);
                    }
                },
                HirStmt::While { cond, body, .. } => {
                    if self.temporal_source_value(cond, &facts.values, false)
                        != Some(InputValue::Bool(false))
                    {
                        let writes = loop_written_symbols(body);
                        let before = facts.loop_values_before(&writes);
                        for symbol in &writes {
                            facts.remove_value(*symbol);
                        }
                        self.temporal_writes(body, roots, trusted, facts);
                        facts.retain_loop_values(before);
                    }
                }
                HirStmt::For { body, .. } => {
                    let writes = loop_written_symbols(body);
                    let before = facts.loop_values_before(&writes);
                    for symbol in &writes {
                        facts.remove_value(*symbol);
                    }
                    self.temporal_writes(body, roots, trusted, facts);
                    facts.retain_loop_values(before);
                }
                _ => {}
            }
        }
    }

    fn temporal_source_value(
        &self,
        expr: &HirExpr,
        values: &HashMap<SymbolId, InputValue>,
        trusted_strengthening: bool,
    ) -> Option<InputValue> {
        let closed = close_expr(self, expr, values, 0, trusted_strengthening)?;
        self.const_input_value_from_hir(&closed)
    }

    fn requalify_temporal_body(
        &mut self,
        body: &mut [HirStmt],
        promoted: &Roots,
        static_requests: bool,
        inherited_conditional: bool,
        reported: &mut HashSet<(Span, String, String)>,
    ) {
        for statement in body {
            match statement {
                HirStmt::If {
                    cond,
                    then_branch,
                    else_branch,
                    ..
                } => {
                    let affected = requalify(cond, promoted);
                    self.check_temporal_request_expr(
                        cond,
                        static_requests,
                        inherited_conditional,
                        promoted,
                        reported,
                    );
                    self.requalify_temporal_body(
                        then_branch,
                        promoted,
                        static_requests,
                        inherited_conditional || affected,
                        reported,
                    );
                    self.requalify_temporal_body(
                        else_branch,
                        promoted,
                        static_requests,
                        inherited_conditional || affected,
                        reported,
                    );
                }
                HirStmt::For {
                    start,
                    end,
                    step,
                    body,
                    ..
                } => {
                    let affected = requalify(start, promoted)
                        | requalify(end, promoted)
                        | requalify(step, promoted);
                    for bound in [start, end, step] {
                        self.check_temporal_request_expr(
                            bound,
                            static_requests,
                            inherited_conditional,
                            promoted,
                            reported,
                        );
                    }
                    self.requalify_temporal_body(
                        body,
                        promoted,
                        static_requests,
                        inherited_conditional || affected,
                        reported,
                    );
                }
                HirStmt::While { cond, body, .. } => {
                    let affected = requalify(cond, promoted);
                    self.check_temporal_request_expr(
                        cond,
                        static_requests,
                        inherited_conditional,
                        promoted,
                        reported,
                    );
                    self.requalify_temporal_body(
                        body,
                        promoted,
                        static_requests,
                        inherited_conditional || affected,
                        reported,
                    );
                }
                HirStmt::Plot {
                    slot,
                    value,
                    colors,
                    ..
                } => {
                    requalify(value, promoted);
                    self.check_temporal_request_expr(
                        value,
                        static_requests,
                        inherited_conditional,
                        promoted,
                        reported,
                    );
                    for (_, color) in colors.iter_mut() {
                        requalify(color, promoted);
                        self.check_temporal_request_expr(
                            color,
                            static_requests,
                            inherited_conditional,
                            promoted,
                            reported,
                        );
                    }
                    if self.plots[slot.0 as usize].style.kind == OutputDisplayKind::Plot
                        && colors
                            .iter()
                            .any(|(_, color)| color.qualifier == Qualifier::Series)
                    {
                        self.plots[slot.0 as usize].style.plot_color_mode =
                            Some(pinewasm_hir::PlotColorMode::Series);
                    }
                }
                HirStmt::Fill { slot, colors, .. } => {
                    for (_, color) in colors.iter_mut() {
                        requalify(color, promoted);
                        self.check_temporal_request_expr(
                            color,
                            static_requests,
                            inherited_conditional,
                            promoted,
                            reported,
                        );
                    }
                    if colors
                        .iter()
                        .any(|(_, color)| color.qualifier == Qualifier::Series)
                    {
                        self.fills[slot.0 as usize].style.fill_color_mode =
                            Some(pinewasm_hir::FillColorMode::Series);
                    }
                }
                other => statement_exprs_mut(other, &mut |expr| {
                    requalify(expr, promoted);
                    self.check_temporal_request_expr(
                        expr,
                        static_requests,
                        inherited_conditional,
                        promoted,
                        reported,
                    );
                }),
            }
        }
    }

    fn check_temporal_request_expr(
        &mut self,
        expr: &HirExpr,
        static_requests: bool,
        conditional: bool,
        promoted: &Roots,
        reported: &mut HashSet<(Span, String, String)>,
    ) {
        if !static_requests {
            return;
        }
        fn depends(expr: &HirExpr, promoted: &Roots) -> bool {
            let mut found = false;
            visit_expr(expr, &mut |child| {
                found |= matches!(child.kind, HirExprKind::ReadVar { symbol, .. } | HirExprKind::ReadVarDynamic { symbol, .. } if promoted.contains(&symbol));
            });
            found
        }
        fn collect(expr: &HirExpr, conditional: bool, promoted: &Roots, spans: &mut Vec<Span>) {
            if conditional
                && matches!(
                    expr.kind,
                    HirExprKind::ReadSecurity { .. }
                        | HirExprKind::ReadSecurityExpr { .. }
                        | HirExprKind::ReadRequest { .. }
                        | HirExprKind::ReadSecurityLowerTf { .. }
                        | HirExprKind::BuildFootprint { .. }
                )
            {
                spans.push(expr.span);
            }
            match &expr.kind {
                HirExprKind::Ternary {
                    cond,
                    then_e,
                    else_e,
                } => {
                    collect(cond, conditional, promoted, spans);
                    let branch = conditional || depends(cond, promoted);
                    collect(then_e, branch, promoted, spans);
                    collect(else_e, branch, promoted, spans);
                }
                HirExprKind::Binary {
                    op: BinOp::And | BinOp::Or,
                    lhs,
                    rhs,
                } => {
                    collect(lhs, conditional, promoted, spans);
                    collect(rhs, conditional || depends(lhs, promoted), promoted, spans);
                }
                _ => expr_children(expr, &mut |child| {
                    collect(child, conditional, promoted, spans);
                }),
            }
        }
        let mut spans = Vec::new();
        collect(expr, conditional, promoted, &mut spans);
        for span in spans {
            if reported.insert((span, "request".to_string(), "control".to_string())) {
                self.error(codes::E0252, "cannot prove that this persistent control is fixed for a non-dynamic request", span,
                    "this local compiler limitation requires an independently fixed request control");
            }
        }
    }
}

#[derive(Default)]
struct Facts {
    values: HashMap<SymbolId, InputValue>,
    writes: HashMap<SymbolId, Span>,
    /// Prior values for sparse branch rollback, in mutation order.
    value_undo: Vec<(SymbolId, Option<InputValue>)>,
    /// Prior write locations for sparse branch rollback, in mutation order.
    write_undo: Vec<(SymbolId, Option<Span>)>,
}

struct ValueDelta {
    values: HashMap<SymbolId, Option<InputValue>>,
    #[cfg(test)]
    scanned_journal_entries: usize,
}

struct WriteDelta {
    writes: HashMap<SymbolId, Option<Span>>,
    #[cfg(test)]
    scanned_journal_entries: usize,
}

impl Facts {
    fn record_write(&mut self, symbol: SymbolId, span: Span) {
        if !self.writes.contains_key(&symbol) {
            self.set_write(symbol, span);
        }
    }

    fn set_write(&mut self, symbol: SymbolId, span: Span) {
        if self.writes.get(&symbol) == Some(&span) {
            return;
        }
        let previous = self.writes.insert(symbol, span);
        self.write_undo.push((symbol, previous));
    }

    fn write_mark(&self) -> usize {
        self.write_undo.len()
    }

    fn changed_writes_since(&self, mark: usize) -> WriteDelta {
        let mut changed = HashSet::new();
        #[cfg(test)]
        let mut scanned_journal_entries = 0;
        for (symbol, _) in &self.write_undo[mark..] {
            #[cfg(test)]
            {
                scanned_journal_entries += 1;
            }
            changed.insert(*symbol);
        }
        let writes = changed
            .into_iter()
            .map(|symbol| (symbol, self.writes.get(&symbol).copied()))
            .collect();
        WriteDelta {
            writes,
            #[cfg(test)]
            scanned_journal_entries,
        }
    }

    fn rollback_writes_to(&mut self, mark: usize) {
        while self.write_undo.len() > mark {
            let (symbol, previous) = self
                .write_undo
                .pop()
                .expect("write journal length was checked");
            match previous {
                Some(span) => {
                    self.writes.insert(symbol, span);
                }
                None => {
                    self.writes.remove(&symbol);
                }
            }
        }
    }

    fn set_value(&mut self, symbol: SymbolId, value: InputValue) {
        if self.values.get(&symbol) == Some(&value) {
            return;
        }
        let previous = self.values.insert(symbol, value);
        self.value_undo.push((symbol, previous));
    }

    fn remove_value(&mut self, symbol: SymbolId) {
        if let Some(previous) = self.values.remove(&symbol) {
            self.value_undo.push((symbol, Some(previous)));
        }
    }

    fn value_mark(&self) -> usize {
        self.value_undo.len()
    }

    fn changed_values_since(&self, mark: usize) -> ValueDelta {
        let mut changed = HashSet::new();
        #[cfg(test)]
        let mut scanned_journal_entries = 0;
        for (symbol, _) in &self.value_undo[mark..] {
            #[cfg(test)]
            {
                scanned_journal_entries += 1;
            }
            changed.insert(*symbol);
        }
        // The snapshot is proportional to changed bindings, never to the
        // number of facts accumulated before this branch.
        let values = changed
            .into_iter()
            .map(|symbol| (symbol, self.values.get(&symbol).cloned()))
            .collect();
        ValueDelta {
            values,
            #[cfg(test)]
            scanned_journal_entries,
        }
    }

    fn rollback_values_to(&mut self, mark: usize) {
        while self.value_undo.len() > mark {
            let (symbol, previous) = self
                .value_undo
                .pop()
                .expect("value journal length was checked");
            match previous {
                Some(value) => {
                    self.values.insert(symbol, value);
                }
                None => {
                    self.values.remove(&symbol);
                }
            }
        }
    }

    fn merge_branch_values(&mut self, then_values: ValueDelta, else_values: ValueDelta) {
        let changed: HashSet<_> = then_values
            .values
            .keys()
            .chain(else_values.values.keys())
            .copied()
            .collect();
        for symbol in changed {
            let before = self.values.get(&symbol).cloned();
            let then_value = then_values.values.get(&symbol).unwrap_or(&before);
            let else_value = else_values.values.get(&symbol).unwrap_or(&before);
            let merged = if then_value == else_value {
                then_value.clone()
            } else {
                None
            };
            if let Some(value) = merged {
                self.set_value(symbol, value);
            } else {
                self.remove_value(symbol);
            }
        }
    }

    fn merge_branch_writes(&mut self, then_writes: WriteDelta, else_writes: WriteDelta) {
        let changed: HashSet<_> = then_writes
            .writes
            .keys()
            .chain(else_writes.writes.keys())
            .copied()
            .collect();
        for symbol in changed {
            let merged = else_writes
                .writes
                .get(&symbol)
                .copied()
                .flatten()
                .or_else(|| then_writes.writes.get(&symbol).copied().flatten())
                .or_else(|| self.writes.get(&symbol).copied());
            if let Some(span) = merged {
                self.set_write(symbol, span);
            }
        }
    }

    fn loop_values_before(
        &self,
        writes: &HashSet<SymbolId>,
    ) -> HashMap<SymbolId, Option<InputValue>> {
        // Loops can invalidate only assigned bindings, so do not copy the
        // entire source-closed value map.
        writes
            .iter()
            .map(|symbol| (*symbol, self.values.get(symbol).cloned()))
            .collect()
    }

    fn retain_loop_values(&mut self, before: HashMap<SymbolId, Option<InputValue>>) {
        for (symbol, value) in before {
            if self.values.get(&symbol) != value.as_ref() {
                self.remove_value(symbol);
            }
        }
    }
}

fn loop_written_symbols(body: &[HirStmt]) -> HashSet<SymbolId> {
    let mut writes = HashSet::new();
    collect_loop_writes(body, &mut writes);
    writes
}

fn collect_loop_writes(body: &[HirStmt], writes: &mut HashSet<SymbolId>) {
    for statement in body {
        match statement {
            HirStmt::Assign { symbol, .. } => {
                writes.insert(*symbol);
            }
            HirStmt::Destructure { targets, .. } => {
                writes.extend(targets.iter().copied());
            }
            HirStmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                collect_loop_writes(then_branch, writes);
                collect_loop_writes(else_branch, writes);
            }
            HirStmt::For { body, .. } | HirStmt::While { body, .. } => {
                collect_loop_writes(body, writes);
            }
            _ => {}
        }
    }
}

fn source_closed_temporal_value(
    analyzer: &Analyzer<'_>,
    expr: &HirExpr,
    values: &HashMap<SymbolId, InputValue>,
) -> Option<InputValue> {
    let closed = close_expr(analyzer, expr, values, 0, false)?;
    analyzer.const_input_value_from_hir(&closed)
}

fn close_expr(
    analyzer: &Analyzer<'_>,
    expr: &HirExpr,
    values: &HashMap<SymbolId, InputValue>,
    depth: u32,
    trusted_strengthening: bool,
) -> Option<HirExpr> {
    close_expr_with_overlays(
        analyzer,
        expr,
        values,
        &mut Vec::new(),
        depth,
        trusted_strengthening,
    )
}

fn close_expr_with_overlays(
    analyzer: &Analyzer<'_>,
    expr: &HirExpr,
    values: &HashMap<SymbolId, InputValue>,
    overlays: &mut Vec<HashMap<SymbolId, Option<InputValue>>>,
    depth: u32,
    trusted_strengthening: bool,
) -> Option<HirExpr> {
    if depth >= MAX_ANALYSIS_NESTING {
        return None;
    }
    let kind = match &expr.kind {
        HirExprKind::ConstInt(_)
        | HirExprKind::ConstFloat(_)
        | HirExprKind::ConstBool(_)
        | HirExprKind::ConstString(_)
        | HirExprKind::ConstColor { .. }
        | HirExprKind::ConstNa
            if expr.qualifier == Qualifier::Const || trusted_strengthening =>
        {
            expr.kind.clone()
        }
        HirExprKind::ReadVar { symbol, offset: 0 } => {
            match closed_input_value(*symbol, values, overlays)? {
                InputValue::Int(v) => HirExprKind::ConstInt(*v),
                InputValue::Float(v) => HirExprKind::ConstFloat(*v),
                InputValue::Bool(v) => HirExprKind::ConstBool(*v),
                InputValue::String(v) => HirExprKind::ConstString(v.clone()),
                InputValue::Color { r, g, b, t } => HirExprKind::ConstColor {
                    r: *r,
                    g: *g,
                    b: *b,
                    t: *t,
                },
                InputValue::Enum { index, .. } => HirExprKind::ConstInt(i64::from(*index)),
                InputValue::Source(_) | InputValue::ExternalSource(_) => return None,
            }
        }
        HirExprKind::Unary { op, operand } => HirExprKind::Unary {
            op: *op,
            operand: Box::new(close_expr_with_overlays(
                analyzer,
                operand,
                values,
                overlays,
                depth + 1,
                trusted_strengthening,
            )?),
        },
        HirExprKind::Binary { op, lhs, rhs } => {
            let lhs = close_expr_with_overlays(
                analyzer,
                lhs,
                values,
                overlays,
                depth + 1,
                trusted_strengthening,
            )?;
            let value = analyzer.const_input_value_from_hir(&lhs);
            if matches!(
                (op, value),
                (BinOp::And, Some(InputValue::Bool(false)))
                    | (BinOp::Or, Some(InputValue::Bool(true)))
            ) {
                return Some(lhs);
            }
            HirExprKind::Binary {
                op: *op,
                lhs: Box::new(lhs),
                rhs: Box::new(close_expr_with_overlays(
                    analyzer,
                    rhs,
                    values,
                    overlays,
                    depth + 1,
                    trusted_strengthening,
                )?),
            }
        }
        HirExprKind::Ternary {
            cond,
            then_e,
            else_e,
        } => {
            let condition = close_expr_with_overlays(
                analyzer,
                cond,
                values,
                overlays,
                depth + 1,
                trusted_strengthening,
            )?;
            let chosen = match analyzer.const_input_value_from_hir(&condition)? {
                InputValue::Bool(true) => then_e,
                InputValue::Bool(false) => else_e,
                _ => return None,
            };
            return close_expr_with_overlays(
                analyzer,
                chosen,
                values,
                overlays,
                depth + 1,
                trusted_strengthening,
            );
        }
        HirExprKind::IntToFloat(inner) => {
            HirExprKind::IntToFloat(Box::new(close_expr_with_overlays(
                analyzer,
                inner,
                values,
                overlays,
                depth + 1,
                trusted_strengthening,
            )?))
        }
        HirExprKind::CallPure { func, args } => HirExprKind::CallPure {
            func: *func,
            args: args
                .iter()
                .map(|arg| {
                    close_expr_with_overlays(
                        analyzer,
                        arg,
                        values,
                        overlays,
                        depth + 1,
                        trusted_strengthening,
                    )
                })
                .collect::<Option<Vec<_>>>()?,
        },
        HirExprKind::EvalSequence { bindings, result } => {
            overlays.push(HashMap::with_capacity(bindings.len()));
            for (slot, value) in bindings {
                let value =
                    close_expr_with_overlays(analyzer, value, values, overlays, depth + 1, false)
                        .and_then(|value| analyzer.const_input_value_from_hir(&value));
                overlays
                    .last_mut()
                    .expect("EvalSequence value scope was pushed")
                    .insert(SymbolId(slot.0), value);
            }
            let result = close_expr_with_overlays(
                analyzer,
                result,
                values,
                overlays,
                depth + 1,
                trusted_strengthening,
            );
            overlays.pop();
            return result;
        }
        // Inputs, chart context, history, collections, state, side effects and
        // unknown symbols do not acquire facts from a cached initializer.
        _ => return None,
    };
    Some(HirExpr {
        ty: expr.ty.clone(),
        qualifier: Qualifier::Const,
        span: expr.span,
        kind,
    })
}

fn closed_input_value<'a>(
    symbol: SymbolId,
    values: &'a HashMap<SymbolId, InputValue>,
    overlays: &'a [HashMap<SymbolId, Option<InputValue>>],
) -> Option<&'a InputValue> {
    for overlay in overlays.iter().rev() {
        if let Some(value) = overlay.get(&symbol) {
            return value.as_ref();
        }
    }
    values.get(&symbol)
}

fn requalify(expr: &mut HirExpr, promoted: &Roots) -> bool {
    if let HirExprKind::EvalSequence { bindings, result } = &mut expr.kind {
        let mut local = promoted.clone();
        for (slot, value) in bindings {
            if requalify(value, &local) {
                local.insert(SymbolId(slot.0));
            } else {
                local.remove(&SymbolId(slot.0));
            }
        }
        let affected = requalify(result, &local);
        // Captures still run, but an unused argument does not determine the
        // returned value's qualifier or its Style color classification.
        if affected {
            expr.qualifier = Qualifier::Series;
        }
        return affected;
    }
    let mut affected = matches!(expr.kind, HirExprKind::ReadVar { symbol, .. } | HirExprKind::ReadVarDynamic { symbol, .. } if promoted.contains(&symbol));
    expr_children_mut(expr, &mut |child| {
        affected |= requalify(child, promoted);
    });
    if affected {
        expr.qualifier = Qualifier::Series;
    }
    affected
}

fn visit_expr(expr: &HirExpr, visitor: &mut dyn FnMut(&HirExpr)) {
    visitor(expr);
    expr_children(expr, &mut |child| visit_expr(child, visitor));
}

fn expr_children(expr: &HirExpr, visitor: &mut dyn FnMut(&HirExpr)) {
    match &expr.kind {
        HirExprKind::Unary { operand, .. }
        | HirExprKind::IntToFloat(operand)
        | HirExprKind::MaterializeHistory { expr: operand, .. }
        | HirExprKind::CopyObject { obj: operand, .. }
        | HirExprKind::GetField { obj: operand, .. }
        | HirExprKind::RegisterDrawing { inner: operand, .. }
        | HirExprKind::DeleteDrawing { id: operand, .. }
        | HirExprKind::ReadBuiltinDynamic {
            offset: operand, ..
        }
        | HirExprKind::ReadVarDynamic {
            offset: operand, ..
        }
        | HirExprKind::TaCall {
            source: operand, ..
        } => visitor(operand),
        HirExprKind::Binary { lhs, rhs, .. }
        | HirExprKind::TaCallDynamic {
            source: lhs,
            length: rhs,
            ..
        }
        | HirExprKind::FootprintRowByPrice {
            footprint: lhs,
            price: rhs,
        }
        | HirExprKind::SetFieldExpr {
            obj: lhs,
            value: rhs,
            ..
        }
        | HirExprKind::LineGetPrice {
            obj: lhs, x: rhs, ..
        }
        | HirExprKind::MaterializeHistoryDynamic {
            expr: lhs,
            offset: rhs,
            ..
        } => {
            visitor(lhs);
            visitor(rhs);
        }
        HirExprKind::Ternary {
            cond,
            then_e,
            else_e,
        } => {
            visitor(cond);
            visitor(then_e);
            visitor(else_e);
        }
        HirExprKind::CallPure { args, .. }
        | HirExprKind::CallTaState { args, .. }
        | HirExprKind::CallCollection { args, .. }
        | HirExprKind::CallPivotPointLevels { args }
        | HirExprKind::NewObject { fields: args, .. }
        | HirExprKind::Tuple { elements: args } => {
            for arg in args {
                visitor(arg);
            }
        }
        HirExprKind::EvalSequence { bindings, result } => {
            for (_, value) in bindings {
                visitor(value);
            }
            visitor(result);
        }
        HirExprKind::SetFields { obj, writes, .. } => {
            visitor(obj);
            for (_, value) in writes {
                visitor(value);
            }
        }
        HirExprKind::SetCell {
            obj,
            column,
            row,
            writes,
            ..
        } => {
            visitor(obj);
            visitor(column);
            visitor(row);
            for (_, value) in writes {
                visitor(value);
            }
        }
        HirExprKind::ClearCells {
            obj,
            start_column,
            start_row,
            end_column,
            end_row,
            ..
        }
        | HirExprKind::MergeCells {
            obj,
            start_column,
            start_row,
            end_column,
            end_row,
            ..
        } => {
            visitor(obj);
            visitor(start_column);
            visitor(start_row);
            visitor(end_column);
            visitor(end_row);
        }
        HirExprKind::ConstInt(_)
        | HirExprKind::ConstFloat(_)
        | HirExprKind::ConstBool(_)
        | HirExprKind::ConstString(_)
        | HirExprKind::ConstColor { .. }
        | HirExprKind::ConstNa
        | HirExprKind::ReadBuiltin { .. }
        | HirExprKind::ReadVar { .. }
        | HirExprKind::ReadInput { .. }
        | HirExprKind::ReadSecurity { .. }
        | HirExprKind::ReadSecurityExpr { .. }
        | HirExprKind::ReadRequest { .. }
        | HirExprKind::BuildFootprint { .. }
        | HirExprKind::ReadSecurityLowerTf { .. }
        | HirExprKind::ReadPivotLevels { .. }
        | HirExprKind::ReadTimeTf { .. }
        | HirExprKind::ReadDrawingsAll { .. } => {}
    }
}

fn expr_children_mut(expr: &mut HirExpr, visitor: &mut dyn FnMut(&mut HirExpr)) {
    match &mut expr.kind {
        HirExprKind::Unary { operand, .. }
        | HirExprKind::IntToFloat(operand)
        | HirExprKind::MaterializeHistory { expr: operand, .. }
        | HirExprKind::CopyObject { obj: operand, .. }
        | HirExprKind::GetField { obj: operand, .. }
        | HirExprKind::RegisterDrawing { inner: operand, .. }
        | HirExprKind::DeleteDrawing { id: operand, .. }
        | HirExprKind::ReadBuiltinDynamic {
            offset: operand, ..
        }
        | HirExprKind::ReadVarDynamic {
            offset: operand, ..
        }
        | HirExprKind::TaCall {
            source: operand, ..
        } => visitor(operand),
        HirExprKind::Binary { lhs, rhs, .. }
        | HirExprKind::TaCallDynamic {
            source: lhs,
            length: rhs,
            ..
        }
        | HirExprKind::FootprintRowByPrice {
            footprint: lhs,
            price: rhs,
        }
        | HirExprKind::SetFieldExpr {
            obj: lhs,
            value: rhs,
            ..
        }
        | HirExprKind::LineGetPrice {
            obj: lhs, x: rhs, ..
        }
        | HirExprKind::MaterializeHistoryDynamic {
            expr: lhs,
            offset: rhs,
            ..
        } => {
            visitor(lhs);
            visitor(rhs);
        }
        HirExprKind::Ternary {
            cond,
            then_e,
            else_e,
        } => {
            visitor(cond);
            visitor(then_e);
            visitor(else_e);
        }
        HirExprKind::CallPure { args, .. }
        | HirExprKind::CallTaState { args, .. }
        | HirExprKind::CallCollection { args, .. }
        | HirExprKind::CallPivotPointLevels { args }
        | HirExprKind::NewObject { fields: args, .. }
        | HirExprKind::Tuple { elements: args } => {
            for arg in args {
                visitor(arg);
            }
        }
        HirExprKind::EvalSequence { bindings, result } => {
            for (_, value) in bindings {
                visitor(value);
            }
            visitor(result);
        }
        HirExprKind::SetFields { obj, writes, .. } => {
            visitor(obj);
            for (_, value) in writes {
                visitor(value);
            }
        }
        HirExprKind::SetCell {
            obj,
            column,
            row,
            writes,
            ..
        } => {
            visitor(obj);
            visitor(column);
            visitor(row);
            for (_, value) in writes {
                visitor(value);
            }
        }
        HirExprKind::ClearCells {
            obj,
            start_column,
            start_row,
            end_column,
            end_row,
            ..
        }
        | HirExprKind::MergeCells {
            obj,
            start_column,
            start_row,
            end_column,
            end_row,
            ..
        } => {
            visitor(obj);
            visitor(start_column);
            visitor(start_row);
            visitor(end_column);
            visitor(end_row);
        }
        HirExprKind::ConstInt(_)
        | HirExprKind::ConstFloat(_)
        | HirExprKind::ConstBool(_)
        | HirExprKind::ConstString(_)
        | HirExprKind::ConstColor { .. }
        | HirExprKind::ConstNa
        | HirExprKind::ReadBuiltin { .. }
        | HirExprKind::ReadVar { .. }
        | HirExprKind::ReadInput { .. }
        | HirExprKind::ReadSecurity { .. }
        | HirExprKind::ReadSecurityExpr { .. }
        | HirExprKind::ReadRequest { .. }
        | HirExprKind::BuildFootprint { .. }
        | HirExprKind::ReadSecurityLowerTf { .. }
        | HirExprKind::ReadPivotLevels { .. }
        | HirExprKind::ReadTimeTf { .. }
        | HirExprKind::ReadDrawingsAll { .. } => {}
    }
}

fn statement_exprs_mut(statement: &mut HirStmt, visitor: &mut dyn FnMut(&mut HirExpr)) {
    match statement {
        HirStmt::Assign { value, .. }
        | HirStmt::Destructure { value, .. }
        | HirStmt::Eval { expr: value, .. }
        | HirStmt::RuntimeError { message: value, .. } => visitor(value),
        HirStmt::SetField { obj, value, .. } => {
            visitor(obj);
            visitor(value);
        }
        HirStmt::Visual { colors, .. } | HirStmt::Fill { colors, .. } => {
            for (_, color) in colors {
                visitor(color);
            }
        }
        HirStmt::Log { format, args, .. } => {
            visitor(format);
            for arg in args {
                visitor(arg);
            }
        }
        HirStmt::Alert { message, freq, .. } => {
            visitor(message);
            visitor(freq);
        }
        HirStmt::AlertCondition {
            condition,
            title,
            message,
            ..
        } => {
            visitor(condition);
            visitor(title);
            visitor(message);
        }
        HirStmt::Order { operands, .. } => {
            for operand in operands {
                visitor(operand);
            }
        }
        HirStmt::Plot { .. }
        | HirStmt::If { .. }
        | HirStmt::For { .. }
        | HirStmt::While { .. }
        | HirStmt::Break { .. }
        | HirStmt::Continue { .. } => {}
    }
}

#[cfg(test)]
mod journal_tests {
    use super::*;
    use pinewasm_span::SourceId;

    fn int(value: i64) -> InputValue {
        InputValue::Int(value)
    }

    #[test]
    fn sparse_branch_join_matches_value_intersection() {
        let stable = SymbolId(0);
        let shared = SymbolId(1);
        let diverged = SymbolId(2);
        let then_only = SymbolId(3);
        let else_only = SymbolId(4);
        let written_in_both = SymbolId(5);
        let then_span = Span::new(SourceId::MAIN, 11, 12);
        let else_span = Span::new(SourceId::MAIN, 21, 22);
        let mut facts = Facts::default();
        facts.set_value(stable, int(0));
        facts.set_value(shared, int(1));
        facts.set_value(diverged, int(2));

        let mark = facts.value_mark();
        facts.set_value(shared, int(9));
        facts.remove_value(diverged);
        facts.set_value(then_only, int(10));
        let then_values = facts.changed_values_since(mark);
        facts.rollback_values_to(mark);

        facts.set_value(shared, int(9));
        facts.set_value(diverged, int(3));
        facts.set_value(else_only, int(11));
        let else_values = facts.changed_values_since(mark);
        facts.rollback_values_to(mark);

        let write_mark = facts.write_mark();
        facts.record_write(written_in_both, then_span);
        let then_writes = facts.changed_writes_since(write_mark);
        facts.rollback_writes_to(write_mark);
        facts.record_write(written_in_both, else_span);
        let else_writes = facts.changed_writes_since(write_mark);
        facts.rollback_writes_to(write_mark);

        facts.merge_branch_values(then_values, else_values);
        facts.merge_branch_writes(then_writes, else_writes);
        assert_eq!(facts.values.get(&stable), Some(&int(0)));
        assert_eq!(facts.values.get(&shared), Some(&int(9)));
        assert!(!facts.values.contains_key(&diverged));
        assert!(!facts.values.contains_key(&then_only));
        assert!(!facts.values.contains_key(&else_only));
        assert_eq!(facts.writes.get(&written_in_both), Some(&else_span));
    }

    #[test]
    fn branch_delta_work_tracks_mutations_not_accumulated_fact_count() {
        let mut facts = Facts::default();
        for raw_symbol in 0..40_000 {
            facts.set_value(SymbolId(raw_symbol), int(i64::from(raw_symbol)));
            facts.set_write(
                SymbolId(raw_symbol),
                Span::new(SourceId::MAIN, raw_symbol, raw_symbol + 1),
            );
        }
        let mark = facts.value_mark();
        let write_mark = facts.write_mark();
        facts.set_value(SymbolId(17), int(-17));
        facts.remove_value(SymbolId(18));
        facts.record_write(SymbolId(40_000), Span::new(SourceId::MAIN, 40_000, 40_001));
        facts.record_write(SymbolId(40_001), Span::new(SourceId::MAIN, 40_001, 40_002));

        let delta = facts.changed_values_since(mark);
        let write_delta = facts.changed_writes_since(write_mark);
        assert_eq!(delta.values.len(), 2);
        assert_eq!(delta.scanned_journal_entries, 2);
        assert_eq!(write_delta.writes.len(), 2);
        assert_eq!(write_delta.scanned_journal_entries, 2);
        facts.rollback_values_to(mark);
        facts.rollback_writes_to(write_mark);
        assert_eq!(facts.values.len(), 40_000);
        assert_eq!(facts.writes.len(), 40_000);
    }

    #[test]
    fn loop_snapshots_and_restores_only_written_symbols() {
        let mut facts = Facts::default();
        for raw_symbol in 0..40_000 {
            facts.set_value(SymbolId(raw_symbol), int(i64::from(raw_symbol)));
        }
        let stable_write = SymbolId(17);
        let changing_write = SymbolId(18);
        let untouched = SymbolId(19);
        let writes = HashSet::from([stable_write, changing_write]);
        let before = facts.loop_values_before(&writes);
        // The accumulated map is 40,000 entries, but this loop mutates two.
        assert_eq!(before.len(), 2);

        for symbol in &writes {
            facts.remove_value(*symbol);
        }
        facts.set_value(stable_write, int(17));
        facts.set_value(changing_write, int(-18));
        facts.retain_loop_values(before);

        assert_eq!(facts.values.get(&stable_write), Some(&int(17)));
        assert!(!facts.values.contains_key(&changing_write));
        assert_eq!(facts.values.get(&untouched), Some(&int(19)));
        assert_eq!(facts.values.len(), 39_999);
    }

    #[test]
    fn eval_sequence_overlays_preserve_shadowing_and_unknown_bindings() {
        let base_symbol = SymbolId(0);
        let shadowed_symbol = SymbolId(1);
        let unknown_symbol = SymbolId(2);
        let nested_symbol = SymbolId(3);
        let values = HashMap::from([
            (base_symbol, int(10)),
            (shadowed_symbol, int(20)),
            (unknown_symbol, int(30)),
        ]);
        let outer = HashMap::from([(shadowed_symbol, Some(int(21))), (unknown_symbol, None)]);
        let inner = HashMap::from([(shadowed_symbol, None), (nested_symbol, Some(int(40)))]);
        let overlays = [outer, inner];

        assert_eq!(
            closed_input_value(base_symbol, &values, &overlays),
            Some(&int(10))
        );
        assert_eq!(
            closed_input_value(shadowed_symbol, &values, &overlays),
            None
        );
        assert_eq!(closed_input_value(unknown_symbol, &values, &overlays), None);
        assert_eq!(
            closed_input_value(nested_symbol, &values, &overlays),
            Some(&int(40))
        );
    }

    #[test]
    fn nested_eval_sequence_shadowing_pops_back_to_the_parent_then_base() {
        let registry = BuiltinRegistry::standard_ref();
        let analyzer = Analyzer::new(registry, ImportedLibraries::default());
        let symbol = SymbolId(7);
        let span = Span::dummy();
        let constant = |value| HirExpr {
            kind: HirExprKind::ConstInt(value),
            ty: Type::Int,
            qualifier: Qualifier::Const,
            span,
        };
        let read = || HirExpr {
            kind: HirExprKind::ReadVar { symbol, offset: 0 },
            ty: Type::Int,
            qualifier: Qualifier::Simple,
            span,
        };
        let inner = HirExpr {
            kind: HirExprKind::EvalSequence {
                bindings: vec![(SeriesSlot(symbol.0), constant(30))],
                result: Box::new(read()),
            },
            ty: Type::Int,
            qualifier: Qualifier::Simple,
            span,
        };
        let expression = HirExpr {
            kind: HirExprKind::EvalSequence {
                bindings: vec![(SeriesSlot(symbol.0), constant(20))],
                result: Box::new(HirExpr {
                    kind: HirExprKind::Binary {
                        op: BinOp::Add,
                        lhs: Box::new(inner),
                        rhs: Box::new(read()),
                    },
                    ty: Type::Int,
                    qualifier: Qualifier::Simple,
                    span,
                }),
            },
            ty: Type::Int,
            qualifier: Qualifier::Simple,
            span,
        };
        let base = HashMap::from([(symbol, int(10))]);

        let closed = close_expr(&analyzer, &expression, &base, 0, false)
            .expect("nested captured values close to constants");
        let HirExprKind::Binary { lhs, rhs, .. } = closed.kind else {
            panic!("expected the closed binary result");
        };
        assert!(matches!(lhs.kind, HirExprKind::ConstInt(30)));
        assert!(matches!(rhs.kind, HirExprKind::ConstInt(20)));

        let after = close_expr(&analyzer, &read(), &base, 0, false)
            .expect("base scope remains available after the nested captures");
        assert!(matches!(after.kind, HirExprKind::ConstInt(10)));
    }
}
