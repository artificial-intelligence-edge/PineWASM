//! `request.*`, security, and time-function analysis: `request.security` /
//! `request.security_lower_tf` (HTF selector lowering), other host-backed
//! `request.*` reads, pivot-point levels, and `time`/`time_close` calls.

use crate::registry::{request_param_signature, RequestParamKind};
use crate::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RealtimeCondition {
    Always(bool),
    Realtime,
    Historical,
}

impl RealtimeCondition {
    fn negate(self) -> Self {
        match self {
            Self::Always(value) => Self::Always(!value),
            Self::Realtime => Self::Historical,
            Self::Historical => Self::Realtime,
        }
    }

    fn and(self, rhs: Self) -> Self {
        use RealtimeCondition::{Always, Historical, Realtime};
        match (self, rhs) {
            (Always(false), _)
            | (_, Always(false))
            | (Realtime, Historical)
            | (Historical, Realtime) => Always(false),
            (Always(true), other) | (other, Always(true)) => other,
            (Realtime, Realtime) => Realtime,
            (Historical, Historical) => Historical,
        }
    }

    fn or(self, rhs: Self) -> Self {
        use RealtimeCondition::{Always, Historical, Realtime};
        match (self, rhs) {
            (Always(true), _)
            | (_, Always(true))
            | (Realtime, Historical)
            | (Historical, Realtime) => Always(true),
            (Always(false), other) | (other, Always(false)) => other,
            (Realtime, Realtime) => Realtime,
            (Historical, Historical) => Historical,
        }
    }
}

/// Pure HIR expressions can be omitted when a scalar request result lowers
/// independently. Stateful calls and host requests must stay in the rejected
/// prefix path.
fn scalar_prefix_expr_is_pure(expr: &HirExpr) -> bool {
    match &expr.kind {
        HirExprKind::ConstInt(_)
        | HirExprKind::ConstFloat(_)
        | HirExprKind::ConstBool(_)
        | HirExprKind::ConstString(_)
        | HirExprKind::ConstColor { .. }
        | HirExprKind::ConstNa
        | HirExprKind::ReadBuiltin { .. }
        | HirExprKind::ReadVar { .. }
        | HirExprKind::ReadInput { .. } => true,
        HirExprKind::ReadBuiltinDynamic { offset, .. }
        | HirExprKind::ReadVarDynamic { offset, .. }
        | HirExprKind::IntToFloat(offset)
        | HirExprKind::Unary {
            operand: offset, ..
        } => scalar_prefix_expr_is_pure(offset),
        HirExprKind::Binary { lhs, rhs, .. } => {
            scalar_prefix_expr_is_pure(lhs) && scalar_prefix_expr_is_pure(rhs)
        }
        HirExprKind::Ternary {
            cond,
            then_e,
            else_e,
        } => {
            scalar_prefix_expr_is_pure(cond)
                && scalar_prefix_expr_is_pure(then_e)
                && scalar_prefix_expr_is_pure(else_e)
        }
        HirExprKind::CallPure { args, .. } => args.iter().all(scalar_prefix_expr_is_pure),
        _ => false,
    }
}

impl<'r> Analyzer<'r> {
    /// Only input-stable selectors are safe to resolve once for a run. Keep
    /// this grammar narrow: a string/bool input, literals, equality, and
    /// ternary selection. Other host-reactive input expressions remain valid
    /// Pine but are not silently frozen to their default value here.
    fn request_symbol_input_expr_supported(&self, expr: &InputActiveExpr) -> bool {
        match expr {
            InputActiveExpr::Value { value } => {
                matches!(value, InputValue::String(_) | InputValue::Bool(_))
            }
            InputActiveExpr::Na => true,
            InputActiveExpr::Input { input } => self
                .inputs
                .get(*input as usize)
                .is_some_and(|decl| decl.kind == InputKind::String || decl.kind == InputKind::Bool),
            InputActiveExpr::Binary { op, lhs, rhs } => {
                matches!(op, InputActiveBinaryOp::Eq | InputActiveBinaryOp::Ne)
                    && self.request_symbol_input_expr_supported(lhs)
                    && self.request_symbol_input_expr_supported(rhs)
            }
            InputActiveExpr::Ternary {
                condition,
                when_true,
                when_false,
            } => {
                self.request_symbol_input_expr_supported(condition)
                    && self.request_symbol_input_expr_supported(when_true)
                    && self.request_symbol_input_expr_supported(when_false)
            }
            _ => false,
        }
    }

    /// The integer-only subset that can safely feed `str.tostring` in a
    /// run-stable request timeframe. Keeping this separate from the general
    /// host-active call allowlist avoids exposing integer formatting through
    /// unrelated host-reactive metadata.
    fn request_timeframe_int_expr_supported(&self, expr: &InputActiveExpr) -> bool {
        match expr {
            InputActiveExpr::Value {
                value: InputValue::Int(_),
            } => true,
            InputActiveExpr::Input { input } => self
                .inputs
                .get(*input as usize)
                .is_some_and(|decl| decl.kind == InputKind::Int),
            InputActiveExpr::Unary {
                op: InputActiveUnaryOp::Neg,
                operand,
            } => self.request_timeframe_int_expr_supported(operand),
            InputActiveExpr::Binary {
                op:
                    InputActiveBinaryOp::Add
                    | InputActiveBinaryOp::Sub
                    | InputActiveBinaryOp::Mul
                    | InputActiveBinaryOp::Rem,
                lhs,
                rhs,
            } => {
                self.request_timeframe_int_expr_supported(lhs)
                    && self.request_timeframe_int_expr_supported(rhs)
            }
            _ => false,
        }
    }

    /// String fragments accepted inside a timeframe expression. Numeric
    /// inputs become strings only through the default, unformatted
    /// `str.tostring(int)` overload; floats and formatting patterns stay out
    /// because their textual form is not stable enough to key a request selector.
    fn request_timeframe_string_expr_supported(&self, expr: &InputActiveExpr) -> bool {
        match expr {
            InputActiveExpr::Value {
                value: InputValue::String(_),
            } => true,
            InputActiveExpr::Input { input } => self
                .inputs
                .get(*input as usize)
                .is_some_and(|decl| decl.kind == InputKind::String),
            InputActiveExpr::Call { function, args }
                if function == "str.tostring" && args.len() == 1 =>
            {
                self.request_timeframe_int_expr_supported(&args[0])
            }
            InputActiveExpr::Binary {
                op: InputActiveBinaryOp::Add,
                lhs,
                rhs,
            } => {
                self.request_timeframe_string_expr_supported(lhs)
                    && self.request_timeframe_string_expr_supported(rhs)
            }
            InputActiveExpr::Ternary {
                condition,
                when_true,
                when_false,
            } => {
                self.request_symbol_input_expr_supported(condition)
                    && self.request_timeframe_string_expr_supported(when_true)
                    && self.request_timeframe_string_expr_supported(when_false)
            }
            _ => false,
        }
    }

    /// Reuse the bounded `InputActiveExpr` wire representation for request
    /// selectors, adding only the typed integer-to-string conversion that is
    /// valid in a derived timeframe. This map is kept separately from ordinary
    /// input-active expressions so the request-only call is never advertised
    /// as a general host-active function.
    pub(crate) fn request_input_expr_from_hir(&self, expr: &HirExpr) -> Option<InputActiveExpr> {
        match &expr.kind {
            HirExprKind::ReadVar { symbol, offset: 0 } => self
                .request_input_expr_origins
                .get(symbol)
                .cloned()
                .or_else(|| self.input_active_expr_from_hir(expr)),
            HirExprKind::CallPure { func, args }
                if func.name() == "str.tostring" && args.len() == 1 && args[0].ty == Type::Int =>
            {
                Some(InputActiveExpr::Call {
                    function: "str.tostring".to_owned(),
                    args: vec![self.request_input_expr_from_hir(&args[0])?],
                })
            }
            HirExprKind::Binary { op, lhs, rhs } => {
                self.input_active_expr_from_hir(expr).or_else(|| {
                    let op = match op {
                        BinOp::Add => InputActiveBinaryOp::Add,
                        BinOp::Sub => InputActiveBinaryOp::Sub,
                        BinOp::Mul => InputActiveBinaryOp::Mul,
                        BinOp::Div => InputActiveBinaryOp::Div,
                        BinOp::Rem => InputActiveBinaryOp::Rem,
                        BinOp::Lt => InputActiveBinaryOp::Lt,
                        BinOp::Le => InputActiveBinaryOp::Le,
                        BinOp::Gt => InputActiveBinaryOp::Gt,
                        BinOp::Ge => InputActiveBinaryOp::Ge,
                        BinOp::Eq => InputActiveBinaryOp::Eq,
                        BinOp::Ne => InputActiveBinaryOp::Ne,
                        BinOp::And => InputActiveBinaryOp::And,
                        BinOp::Or => InputActiveBinaryOp::Or,
                    };
                    Some(InputActiveExpr::Binary {
                        op,
                        lhs: Box::new(self.request_input_expr_from_hir(lhs)?),
                        rhs: Box::new(self.request_input_expr_from_hir(rhs)?),
                    })
                })
            }
            HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } => self.input_active_expr_from_hir(expr).or_else(|| {
                Some(InputActiveExpr::Ternary {
                    condition: Box::new(self.request_input_expr_from_hir(cond)?),
                    when_true: Box::new(self.request_input_expr_from_hir(then_e)?),
                    when_false: Box::new(self.request_input_expr_from_hir(else_e)?),
                })
            }),
            _ => self.input_active_expr_from_hir(expr),
        }
    }

    fn input_stable_request_string_for_expr(&self, expr: &HirExpr) -> Option<String> {
        // String overloads can promote input-only expressions to simple.
        // The recovered expression tree below still must contain only the
        // bounded, supported input operations; qualifier alone is not proof.
        if expr.ty != Type::String || expr.qualifier == Qualifier::Series {
            return None;
        }
        if matches!(&expr.kind, HirExprKind::ReadVar { symbol, .. }
            if self.symbols[symbol.0 as usize].persistent
                || self.symbols[symbol.0 as usize].varip)
        {
            return None;
        }
        let tree = self.request_input_expr_from_hir(expr)?;
        if !self.request_symbol_input_expr_supported(&tree)
            && !self.request_timeframe_string_expr_supported(&tree)
        {
            return None;
        }
        Self::encode_request_string_expr(&tree)
    }

    fn encode_request_string_expr(tree: &InputActiveExpr) -> Option<String> {
        let bytes = serde_json::to_vec(tree).ok()?;
        if bytes.len() > 64 * 1024 {
            return None;
        }
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            use std::fmt::Write;
            write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
        }
        Some(format!("@input_expr+{encoded}"))
    }

    /// An empty or chart-symbol string resolves to the chart's own feed
    /// (`syminfo.tickerid`). A constant empty symbol, written directly or
    /// through a const binding, is read as `syminfo.tickerid`, so every later
    /// check treats it as the chart feed.
    fn chart_symbol_for_empty(&self, hir: HirExpr) -> HirExpr {
        let empty = match &hir.kind {
            HirExprKind::ConstString(value) => value.is_empty(),
            _ => matches!(
                self.const_input_value_from_hir(&hir),
                Some(InputValue::String(value)) if value.is_empty()
            ),
        };
        if !empty {
            return hir;
        }
        let series = BuiltinSeries::SyminfoTickerid;
        HirExpr {
            kind: HirExprKind::ReadBuiltin { series, offset: 0 },
            ty: series.value_type(),
            qualifier: series.current_read_qualifier(),
            span: hir.span,
        }
    }

    /// Recover whether an expression denotes the chart's current feed. The two
    /// `syminfo` accessors are runtime-backed simple strings, while this
    /// provenance remains analyzer-only so request lowering does not confuse a
    /// literal string that happens to contain `"syminfo.tickerid"` with the
    /// builtin itself.
    pub(crate) fn current_feed_symbol_for_expr(&self, expr: &HirExpr) -> Option<BuiltinSeries> {
        self.chart_symbol_origin_for_expr(expr).filter(|series| {
            matches!(
                series,
                BuiltinSeries::SyminfoTicker | BuiltinSeries::SyminfoTickerid
            )
        })
    }

    /// Preserve unchanged chart symbol accessors through ordinary aliases.
    pub(crate) fn chart_symbol_origin_for_expr(&self, expr: &HirExpr) -> Option<BuiltinSeries> {
        match &expr.kind {
            HirExprKind::ReadBuiltin {
                series:
                    series @ (BuiltinSeries::SyminfoTicker
                    | BuiltinSeries::SyminfoTickerid
                    | BuiltinSeries::SyminfoBasecurrency),
                offset: 0,
            } => Some(*series),
            HirExprKind::ReadVar { symbol, offset: 0 } => {
                self.current_feed_symbols.get(symbol).copied()
            }
            _ => None,
        }
    }

    /// Encode a chart-base-currency suffix so the host selector can resolve
    /// it from the current chart metadata on each run. Hex encoding keeps any
    /// literal suffix out of the slash-delimited selector grammar.
    fn chart_basecurrency_suffix_for_expr(&self, expr: &HirExpr) -> Option<String> {
        let HirExprKind::Binary {
            op: BinOp::Add,
            lhs,
            rhs,
        } = &expr.kind
        else {
            return None;
        };
        (self.chart_symbol_origin_for_expr(lhs) == Some(BuiltinSeries::SyminfoBasecurrency)).then(
            || match &rhs.kind {
                HirExprKind::ConstString(suffix) => Some(suffix.as_bytes().iter().fold(
                    String::new(),
                    |mut encoded, byte| {
                        use std::fmt::Write;
                        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
                        encoded
                    },
                )),
                _ => None,
            },
        )?
    }

    pub(crate) fn current_chart_timeframe_for_expr(&self, expr: &HirExpr) -> bool {
        match &expr.kind {
            HirExprKind::ReadBuiltin {
                series: BuiltinSeries::TimeframePeriod,
                offset: 0,
            } => true,
            HirExprKind::ReadVar { symbol, offset: 0 } => {
                self.current_chart_timeframes.contains(symbol)
            }
            _ => Self::is_v5_timeframe_period_compat_expr(expr),
        }
    }

    /// Recognize the `timeframe.period` expression form that always denotes the
    /// chart timeframe when the multiplier is one. Every branch denotes the
    /// chart's own timeframe, so a request selector may
    /// use the existing chart-context path without evaluating a string
    /// selector from a bar-dependent expression.
    fn is_v5_timeframe_period_compat_expr(expr: &HirExpr) -> bool {
        fn read(expr: &HirExpr, series: BuiltinSeries) -> bool {
            matches!(
                expr.kind,
                HirExprKind::ReadBuiltin {
                    series: found,
                    offset: 0
                } if found == series
            )
        }
        fn timeframe_period(expr: &HirExpr) -> bool {
            read(expr, BuiltinSeries::TimeframePeriod)
        }
        fn branch(
            expr: &HirExpr,
            series: BuiltinSeries,
            value: &str,
            tail: fn(&HirExpr) -> bool,
        ) -> bool {
            let HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } = &expr.kind
            else {
                return false;
            };
            read(cond, series)
                && matches!(&then_e.kind, HirExprKind::ConstString(found) if found == value)
                && tail(else_e)
        }
        fn seconds(expr: &HirExpr) -> bool {
            branch(
                expr,
                BuiltinSeries::TimeframeIsseconds,
                "S",
                timeframe_period,
            )
        }
        fn monthly(expr: &HirExpr) -> bool {
            branch(expr, BuiltinSeries::TimeframeIsmonthly, "M", seconds)
        }
        fn weekly(expr: &HirExpr) -> bool {
            branch(expr, BuiltinSeries::TimeframeIsweekly, "W", monthly)
        }
        fn daily(expr: &HirExpr) -> bool {
            branch(expr, BuiltinSeries::TimeframeIsdaily, "D", weekly)
        }

        let HirExprKind::Ternary {
            cond,
            then_e,
            else_e,
        } = &expr.kind
        else {
            return false;
        };
        let is_multiplier_not_one = matches!(
            &cond.kind,
            HirExprKind::Binary {
                op: BinOp::Ne,
                lhs,
                rhs,
            } if (read(lhs, BuiltinSeries::TimeframeMultiplier)
                && matches!(rhs.kind, HirExprKind::ConstInt(1)))
                || (read(rhs, BuiltinSeries::TimeframeMultiplier)
                    && matches!(lhs.kind, HirExprKind::ConstInt(1)))
        );
        is_multiplier_not_one && timeframe_period(then_e) && daily(else_e)
    }

    /// Check a `request.security(symbol, timeframe, expression, gaps?,
    /// lookahead?, ignore_invalid_symbol?, currency?, calc_bars_count?)` call.
    /// Produces a `series float`.
    ///
    /// `request.security` is modeled as either an honest same-feed resample of
    /// the chart's own bars to a higher timeframe (see `pinewasm_vm::resample`
    /// and docs/runtime.md), or as a host-provided external-symbol series. The
    /// analyzer therefore enforces:
    ///  - `symbol` must be `syminfo.ticker`/`syminfo.tickerid`, a constant
    ///    string, or an input string. The `syminfo` builtins use the chart's
    ///    own feed; any other resolved string folds into a host selector under
    ///    `MarketData::request_data`;
    ///  - `timeframe` must be a known constant string, `timeframe.period`, or
    ///    a direct string input (including unchanged ordinary aliases);
    ///    constants and selected inputs have a sane range
    ///    (rejecting an absurd/overflowing numeral). The actual chart
    ///    timeframe is a runtime property this analyzer cannot see, so the
    ///    requested timeframe must be higher than the chart timeframe. The VM
    ///    enforces this against the real feed
    ///    (`RuntimeErrorKind::SecurityTimeframeBelowChart`) rather than
    ///    comparing it here against a hard-coded chart timeframe;
    ///  - `expression` is either a plain OHLCV/derived builtin series read
    ///    (`close`, `hl2`, ...) or a compound sub-expression that can lower
    ///    into an `HtfExpr` tree. Same-feed compound expressions execute in a
    ///    secondary context; external-symbol compounds use a canonical host
    ///    selector that identifies the expression and merge modes;
    ///  - `gaps` / `lookahead` (optional) must be the matching `barmerge.*`
    ///    tokens; they default to `gaps_off` / `lookahead_off`. Requesting
    ///    either as `_on` for a plain external-symbol read is rejected
    ///    (`E0242`): that fast path has no HTF bucketing on our side for these
    ///    flags to affect. External compound selectors encode the modes for the
    ///    host to honor;
    ///  - `ignore_invalid_symbol` (optional) must be bool. The network-free
    ///    runtime already represents a missing external host series as `na`,
    ///    which is the observable result requested by `true`.
    fn scalar_request_prefix_can_be_omitted(
        &self,
        prefix: &[HirStmt],
        expression: &HirExpr,
    ) -> bool {
        if self.lower_htf_expr(expression, &mut 0, &mut 0).is_none() {
            return false;
        }
        prefix.iter().all(|statement| match statement {
            HirStmt::Assign { symbol, value, .. } => self
                .symbols
                .get(symbol.0 as usize)
                .is_some_and(|declaration| {
                    !declaration.persistent
                        && !declaration.varip
                        && scalar_prefix_expr_is_pure(value)
                }),
            _ => false,
        })
    }

    pub(crate) fn analyze_security_call(&mut self, args: &[ast::CallArg], span: Span) -> HirExpr {
        self.analyze_security_result(args, span, false)
    }

    pub(crate) fn analyze_security_tuple_call(
        &mut self,
        args: &[ast::CallArg],
        span: Span,
    ) -> TupleRhs {
        let result = self.analyze_security_result(args, span, true);
        match result.kind {
            HirExprKind::Tuple { elements } => TupleRhs::Elements(elements),
            _ => TupleRhs::Error,
        }
    }

    fn analyze_security_result(
        &mut self,
        args: &[ast::CallArg],
        span: Span,
        tuple_result: bool,
    ) -> HirExpr {
        // Bind by position then name, keeping the sparse layout: the optional
        // `gaps`/`lookahead` can be supplied by name independently (a named
        // `lookahead=` must NOT force `gaps` to be present), unlike the dense
        // `bind_positional` helper (which E0204s an interior gap).
        let (bound, overflow) =
            match self.bind_call_args("request.security", &SECURITY_PARAM_NAMES, args) {
                Some(a) => a,
                None => return self.error_expr(span),
            };
        // Arity: the three required params must be present; no overflow.
        let missing_required = bound[0].is_none() || bound[1].is_none() || bound[2].is_none();
        if missing_required || !overflow.is_empty() {
            self.error(
                codes::E0204,
                format!(
                    "`request.security` expects 3 to 8 argument(s), found {}",
                    args.len()
                ),
                span,
                "expected 3 to 8 argument(s) (symbol, timeframe, expression, gaps?, lookahead?, ignore_invalid_symbol?, currency?, calc_bars_count?)",
            );
            for arg in bound.iter().flatten().chain(overflow.iter()) {
                let _ = self.analyze_expr(arg);
            }
            return self.error_expr(span);
        }
        // Required slots are present (checked above).
        let symbol_arg = bound[0].unwrap();
        let timeframe_arg = bound[1].unwrap();
        let expression_arg = bound[2].unwrap();

        // --- symbol: own feed or host-provided external selector -------------
        let symbol_hir = {
            let symbol = self.analyze_expr(symbol_arg);
            self.chart_symbol_for_empty(symbol)
        };
        self.defer_temporal_fixedness_at(
            &symbol_hir,
            "request.security",
            "symbol",
            symbol_arg.span(),
        );
        let mut ok = !symbol_hir.ty.is_error();
        let mut external_symbol: Option<String> = None;
        let current_feed = self.current_feed_symbol_for_expr(&symbol_hir).is_some();
        if ok && !current_feed {
            let symbol_value = match &symbol_hir.kind {
                HirExprKind::ConstString(value) => Some(value.clone()),
                _ => self
                    .const_input_value_from_hir(&symbol_hir)
                    .and_then(|value| match value {
                        InputValue::String(value) => Some(value),
                        _ => None,
                    })
                    .or_else(|| {
                        let origin = self.input_origin_for_expr(&symbol_hir)?;
                        match &self.inputs.get(origin.input as usize)?.defval {
                            InputValue::String(value) => Some(value.clone()),
                            _ => None,
                        }
                    })
                    .or_else(|| {
                        self.chart_basecurrency_suffix_for_expr(&symbol_hir)
                            .map(|suffix| format!("@basecurrency+{suffix}"))
                    })
                    .or_else(|| self.input_stable_request_string_for_expr(&symbol_hir)),
            };
            match symbol_value {
                Some(s) => {
                    external_symbol = Some(s);
                }
                None => {
                    self.error(
                        codes::E0242,
                        "`request.security` symbol must be a const or input string",
                        symbol_arg.span(),
                        "use `syminfo.tickerid`, `input.symbol(...)`, or a literal host-data symbol",
                    );
                    ok = false;
                }
            }
        }

        // --- timeframe: known constant or runtime chart period ----------------
        // The timeframe ordering check needs the ACTUAL chart feed timeframe,
        // which is a runtime property this analyzer cannot see — checking it
        // here against a hard-coded chart timeframe (e.g. assuming a daily
        // chart) would wrongly reject a valid intraday
        // request (chart "5", request "60") and wrongly accept an invalid one
        // (chart "1W", request "1D"). The VM enforces it against the real feed
        // instead (`RuntimeErrorKind::SecurityTimeframeBelowChart`).
        let tf_hir = self.analyze_expr(timeframe_arg);
        self.defer_temporal_fixedness_at(
            &tf_hir,
            "request.security",
            "timeframe",
            timeframe_arg.span(),
        );
        let mut chart_tf = self.current_chart_timeframe_for_expr(&tf_hir);
        let tf_input = if tf_hir.ty == Type::String
            && !matches!(&tf_hir.kind, HirExprKind::ReadVar { symbol, .. }
                if self.symbols[symbol.0 as usize].persistent
                    || self.symbols[symbol.0 as usize].varip)
        {
            self.input_origin_for_expr(&tf_hir)
                .filter(|origin| !origin.inverted)
                .map(|origin| origin.input)
                .or_else(|| self.selected_input_for_expr(&tf_hir))
        } else {
            None
        };
        let mut htf_seconds = 0i64;
        let mut calendar_months = None;
        let mut tf_token = None;
        let mut timeframe_expr = None;
        if !tf_hir.ty.is_error() {
            if chart_tf {
                // Zero is reserved for the chart's actual timeframe. The VM
                // resolves it before validation and resampling; external
                // selectors substitute the chart's timeframe at lookup.
                tf_token = Some("@chart".to_owned());
            } else if let Some(input) = tf_input {
                // Negative lengths encode a run-constant input id. This keeps
                // the existing request opcode's wire shape while the VM
                // resolves the selected timeframe on every run.
                htf_seconds = -i64::from(input) - 1;
                tf_token = Some("@input".to_owned());
            } else {
                let constant = self.const_input_value_from_hir(&tf_hir);
                match constant {
                    Some(InputValue::String(tf)) => {
                        if tf.is_empty() {
                            chart_tf = true;
                            tf_token = Some("@chart".to_owned());
                        } else {
                            let requested = timeframe_seconds(&tf);
                            // Generous sanity ceiling (~100 years). A larger value means
                            // an absurd/overflowing numeral (`timeframe_seconds`
                            // saturates rather than panicking) — reject it cleanly
                            // instead of silently accepting a nonsensical timeframe.
                            const MAX_TF_SECONDS: i64 = 100 * 366 * 86_400;
                            if requested > MAX_TF_SECONDS {
                                self.error(
                                    codes::E0242,
                                    format!("`request.security` timeframe `{tf}` is out of range"),
                                    timeframe_arg.span(),
                                    "use a realistic timeframe (seconds up to months, e.g. \"1D\", \"1W\", \"3M\")",
                                );
                                ok = false;
                            } else {
                                htf_seconds = requested;
                                tf_token = Some(tf.clone());
                                let trimmed = tf.trim();
                                let (number, unit) = trimmed.split_at(
                                    trimmed.char_indices().last().map_or(0, |(index, _)| index),
                                );
                                // Preserve the W unit through the existing serialized
                                // timeframe-expression lane: nominal seconds alone cannot
                                // distinguish calendar weeks from fixed-width 7D blocks.
                                if unit.eq_ignore_ascii_case("W") {
                                    timeframe_expr =
                                        Self::encode_request_string_expr(&InputActiveExpr::Value {
                                            value: InputValue::String(tf.clone()),
                                        });
                                    if timeframe_expr.is_none() {
                                        self.error(
                                            codes::E0242,
                                            "request timeframe encoding is out of range",
                                            timeframe_arg.span(),
                                            "use a bounded timeframe string",
                                        );
                                        ok = false;
                                    }
                                }
                                if unit.eq_ignore_ascii_case("M") {
                                    calendar_months = if number.is_empty() {
                                        Some(1)
                                    } else {
                                        number.parse::<u32>().ok().filter(|months| *months > 0)
                                    };
                                }
                            }
                        }
                    }
                    // A non-string timeframe is a type error (`E0205`), not
                    // one of the request restrictions `E0242` reports.
                    _ if tf_hir.ty != Type::String => {
                        self.error(
                            codes::E0205,
                            format!(
                                "`request.security` argument `timeframe` must be string, found {}",
                                tf_hir.ty.describe()
                            ),
                            timeframe_arg.span(),
                            "expected string",
                        );
                        ok = false;
                    }
                    _ => {
                        if let Some(encoded) = self.input_stable_request_string_for_expr(&tf_hir) {
                            tf_token = Some(encoded.clone());
                            timeframe_expr = Some(encoded);
                        } else {
                            self.error(
                                codes::E0242,
                                "`request.security` timeframe must be a constant string, input string, or `timeframe.period`",
                                timeframe_arg.span(),
                                "pass a timeframe like \"1D\", `input.timeframe(...)`, or `timeframe.period`",
                            );
                            ok = false;
                        }
                    }
                }
            }
        } else {
            ok = false;
        }

        // --- expression: a plain price series OR a compound HTF sub-program --
        // A plain OHLCV/derived read lowers to `ReadSecurity` (fast path); a
        // tractable compound expression (ta.* / arithmetic over the resampled
        // OHLCV) lowers into an `HtfExpr` tree run by a secondary context.
        // Anything the secondary context cannot yet evaluate is
        // rejected with `E0244`.
        let pending_start = self.pending_stmts.len();
        let expr_hirs = if tuple_result {
            // Literal tuples are valid request expressions, but are not a
            // general tuple-declaration RHS in Pine.
            let mut inner = expression_arg;
            while let ast::Expr::Group { inner: grouped, .. } = inner {
                inner = grouped;
            }
            let rhs = if let ast::Expr::Tuple { elements, span } = inner {
                let result = self.analyze_tuple_expr(elements, *span, true);
                match result.kind {
                    HirExprKind::Tuple { elements } => TupleRhs::Elements(elements),
                    _ => TupleRhs::Error,
                }
            } else {
                self.analyze_tuple_rhs(expression_arg, 0)
            };
            match rhs {
                TupleRhs::Elements(elements) => elements,
                TupleRhs::TaCall { .. } => {
                    self.error(
                        codes::E0244,
                        "`request.security` tuple TA expression cannot be evaluated in the secondary context",
                        expression_arg.span(),
                        "tuple-returning TA calls require secondary-context tuple evaluation",
                    );
                    return self.error_expr(span);
                }
                TupleRhs::Error => return self.error_expr(span),
            }
        } else {
            vec![self.analyze_expr(expression_arg)]
        };
        if self.pending_stmts.len() > pending_start {
            // Inlined function locals are origins for the requested-context
            // trees, not instructions to execute on chart bars. Only discard
            // a prefix after bounded pure expansion succeeds. Effects,
            // persistent state and unknown bounds remain unsupported.
            let prefix = self.pending_stmts[pending_start..].to_vec();
            let prefix_supported = self.prepare_htf_prefix(&prefix)
                || (!tuple_result
                    && expr_hirs.first().is_some_and(|expression| {
                        self.scalar_request_prefix_can_be_omitted(&prefix, expression)
                    }));
            if !prefix_supported {
                let (message, note) = if tuple_result {
                    (
                        "request.security tuple function body cannot be evaluated in the secondary context",
                        "supported tuple prefixes contain pure numeric calculations and bounded constant loops",
                    )
                } else {
                    (
                        "request.security scalar function body cannot be evaluated in the secondary context",
                        "scalar helper prefixes must contain supported requested-context calculations without effects or carried state",
                    )
                };
                self.error(codes::E0244, message, expression_arg.span(), note);
                return self.error_expr(span);
            }
            self.pending_stmts.truncate(pending_start);
        }
        enum SecExpr {
            Plain(BuiltinSeries),
            Compound(HtfExpr),
        }
        let mut sec_exprs = Vec::with_capacity(expr_hirs.len());
        let mut result_types = Vec::with_capacity(expr_hirs.len());
        for expr_hir in expr_hirs {
            let result_ty = expr_hir.ty.clone();
            if external_symbol.is_some() && result_ty == Type::Bool {
                self.error(
                    codes::E0244,
                    "`request.security` external host data cannot preserve a boolean expression result",
                    expression_arg.span(),
                    "use the same-feed symbol so the secondary context evaluates the expression locally",
                );
                ok = false;
            }
            if tuple_result
                && !expr_hir.ty.is_error()
                && !matches!(expr_hir.ty, Type::Float | Type::Bool)
            {
                self.error(
                    codes::E0244,
                    "`request.security` tuple components must currently be float or bool expressions",
                    expr_hir.span,
                    "the secondary context cannot yet preserve this tuple component type",
                );
                ok = false;
            }
            let mut sec_expr = SecExpr::Plain(BuiltinSeries::Close);
            if !expr_hir.ty.is_error() {
                match &expr_hir.kind {
                    HirExprKind::ReadBuiltin { series, offset: 0 } if series.is_price_source() => {
                        sec_expr = SecExpr::Plain(*series);
                    }
                    _ => match self.lower_htf_expr(&expr_hir, &mut 0, &mut 0) {
                        Some(tree) => {
                            let tree = if result_ty == Type::Bool {
                                HtfExpr::BoolResult(Box::new(tree))
                            } else {
                                tree
                            };
                            if external_symbol.is_some() && tree.uses_input() {
                                self.error(
                                codes::E0244,
                                "`request.security` external compound expression cannot use input-selected secondary values",
                                expression_arg.span(),
                                "the host-backed selector cannot identify an input-selected secondary-context expression",
                            );
                                ok = false;
                            }
                            // Alias/helper lowering may recover a plain price
                            // source. Keep its canonical host lane and merge
                            // restrictions identical to a direct source read.
                            sec_expr = match tree {
                                HtfExpr::Source(series) => SecExpr::Plain(series),
                                compound => SecExpr::Compound(compound),
                            };
                        }
                        None => {
                            self.error(
                            codes::E0244,
                            "`request.security` compound expression uses a construct the \
                             secondary higher-timeframe context cannot evaluate",
                            expression_arg.span(),
                            "supported: `ta.sma`/`ta.ema`/`ta.change` and arithmetic/\
                             comparison over the resampled OHLCV price series (e.g. `ta.sma(close, 2)`)",
                        );
                            ok = false;
                        }
                    },
                }
            } else {
                ok = false;
            }
            sec_exprs.push(sec_expr);
            result_types.push(result_ty);
        }

        // --- gaps / lookahead: optional barmerge.* tokens --------------------
        let gaps_on = match self.security_barmerge_flag(
            bound.get(3).copied().flatten(),
            "gaps",
            "barmerge.gaps_off",
            "barmerge.gaps_on",
        ) {
            Ok(v) => v,
            Err(()) => {
                ok = false;
                false
            }
        };
        let lookahead_on = match self.security_barmerge_flag(
            bound.get(4).copied().flatten(),
            "lookahead",
            "barmerge.lookahead_off",
            "barmerge.lookahead_on",
        ) {
            Ok(v) => v,
            Err(()) => {
                ok = false;
                false
            }
        };
        if let Some(arg) = bound.get(5).copied().flatten() {
            let hir = self.analyze_expr_expecting(arg, Some(&Type::Bool));
            if !hir.ty.is_error() && hir.ty != Type::Bool {
                self.error(
                    codes::E0205,
                    format!(
                        "`request.security` `ignore_invalid_symbol` must be bool, found {}",
                        hir.ty.describe()
                    ),
                    arg.span(),
                    "pass `true` or `false`",
                );
                ok = false;
            }
        }

        // --- currency: const string, conversion is a host concern ------------
        // The network-free runtime has no FX rates, so a request for anything
        // other than "no conversion" is rejected rather than silently returning
        // unconverted values (the same rule as `gaps`/`lookahead` above).
        if let Some(arg) = bound.get(6).copied().flatten() {
            let hir = self.analyze_expr_expecting(arg, Some(&Type::String));
            if !hir.ty.is_error() {
                self.defer_temporal_fixedness_at(&hir, "request.security", "currency", arg.span());
                match &hir.kind {
                    HirExprKind::ConstString(code) if code == "NONE" => {}
                    HirExprKind::ConstString(code) => {
                        self.error(
                            codes::E0242,
                            format!(
                                "`request.security` cannot convert to `{code}`: the runtime has no exchange rates"
                            ),
                            arg.span(),
                            "omit `currency` or pass `currency.NONE`",
                        );
                        ok = false;
                    }
                    _ => {
                        self.error(
                            codes::E0242,
                            "`request.security` `currency` must be a const string",
                            arg.span(),
                            "pass a `currency.*` constant",
                        );
                        ok = false;
                    }
                }
            }
        }

        // --- calc_bars_count: const int history cap --------------------------
        // A positive cap asks for fewer historical bars than the feed provides.
        // The runtime always evaluates the whole feed, so honoring it would
        // require truncating history; accept the "no limit" value
        // and reject the rest instead of ignoring it.
        if let Some(arg) = bound.get(7).copied().flatten() {
            let hir = self.analyze_expr_expecting(arg, Some(&Type::Int));
            if !hir.ty.is_error() {
                self.defer_temporal_fixedness_at(
                    &hir,
                    "request.security",
                    "calc_bars_count",
                    arg.span(),
                );
                match &hir.kind {
                    HirExprKind::ConstInt(0) => {}
                    _ => {
                        self.error(
                            codes::E0242,
                            "`request.security` `calc_bars_count` is not supported",
                            arg.span(),
                            "omit it or pass `0` (the runtime evaluates the whole feed)",
                        );
                        ok = false;
                    }
                }
            }
        }

        if !ok {
            return self.error_expr(span);
        }
        let mut results = Vec::with_capacity(sec_exprs.len());
        for (sec_expr, result_ty) in sec_exprs.into_iter().zip(result_types) {
            let selector = match (&external_symbol, &sec_expr, tf_token.as_deref()) {
                (Some(symbol), SecExpr::Plain(source), Some(tf)) => {
                    // A host-backed external read returns the host's series
                    // verbatim per chart bar. There is no local HTF bucketing to
                    // honor `gaps` (na between HTF closes) or `lookahead` (repaint
                    // the forming bucket), so reject instead of silently no-oping.
                    if gaps_on || lookahead_on {
                        let flag_span = if gaps_on { bound[3] } else { bound[4] }
                            .map_or(span, pinewasm_syntax::Expr::span);
                        self.error(
                        codes::E0242,
                        "`request.security` `gaps`/`lookahead` are not supported for an external host-backed symbol",
                        flag_span,
                        "host-backed external reads return the host's series verbatim per chart bar; omit `gaps`/`lookahead` (or pass their `_off` defaults), or use `syminfo.tickerid` for same-feed resampling",
                    );
                        return self.error_expr(span);
                    }
                    Some(if chart_tf {
                        format!("security_chart/{symbol}/{}", source.name())
                    } else if let Some(input) = tf_input {
                        format!("security_input/{input}/{symbol}/{}", source.name())
                    } else {
                        format!("security/{symbol}/{tf}/{}", source.name())
                    })
                }
                (Some(_), SecExpr::Compound(_), _) => None,
                _ => None,
            };

            if let (Some(symbol), SecExpr::Compound(expr), Some(timeframe)) =
                (&external_symbol, &sec_expr, tf_token.as_deref())
            {
                let selector = if chart_tf {
                    format!(
                        "security_expression_chart/{}/{}/{}/{}",
                        symbol,
                        if gaps_on { "gaps_on" } else { "gaps_off" },
                        if lookahead_on {
                            "lookahead_on"
                        } else {
                            "lookahead_off"
                        },
                        expr.selector_fragment()
                    )
                } else if let Some(input) = tf_input {
                    format!(
                        "security_expression_input/{}/{}/{}/{}/{}",
                        input,
                        symbol,
                        if gaps_on { "gaps_on" } else { "gaps_off" },
                        if lookahead_on {
                            "lookahead_on"
                        } else {
                            "lookahead_off"
                        },
                        expr.selector_fragment()
                    )
                } else {
                    format!(
                        "security_expression/{}/{}/{}/{}/{}",
                        symbol,
                        timeframe,
                        if gaps_on { "gaps_on" } else { "gaps_off" },
                        if lookahead_on {
                            "lookahead_on"
                        } else {
                            "lookahead_off"
                        },
                        expr.selector_fragment()
                    )
                };
                results.push(HirExpr {
                    kind: HirExprKind::ReadRequest {
                        func: RequestFunc::SecurityExpression,
                        selector,
                    },
                    ty: result_ty,
                    qualifier: Qualifier::Series,
                    span,
                });
                continue;
            }

            let kind = match sec_expr {
                SecExpr::Plain(source) => HirExprKind::ReadSecurity {
                    selector,
                    source,
                    htf_seconds,
                    timeframe_expr: timeframe_expr.clone(),
                    calendar_months,
                    gaps_on,
                    lookahead_on,
                },
                SecExpr::Compound(expr) => HirExprKind::ReadSecurityExpr {
                    expr,
                    htf_seconds,
                    timeframe_expr: timeframe_expr.clone(),
                    calendar_months,
                    gaps_on,
                    lookahead_on,
                },
            };
            results.push(HirExpr {
                kind,
                ty: result_ty,
                qualifier: Qualifier::Series,
                span,
            });
        }
        if tuple_result {
            HirExpr {
                kind: HirExprKind::Tuple { elements: results },
                ty: Type::Void,
                qualifier: Qualifier::Series,
                span,
            }
        } else {
            results.pop().expect("scalar security expression")
        }
    }

    /// Lower an already-checked HIR sub-expression into a compact [`HtfExpr`]
    /// tree for a `request.security` secondary context. Returns `None`
    /// for any construct the secondary context cannot evaluate over the
    /// resampled HTF bars (the caller then rejects the call with `E0244`).
    ///
    /// `next_ta_slot` hands out independent per-site `ta.*` state slots in the
    /// secondary context's OWN state space (never shared with `main`).
    ///
    /// A lowering that fails forgets the default-derived `ta.ema` lengths it
    /// recorded (`request_default_ema_lengths`): the request that would have
    /// kept them is rejected, or another route evaluates the expression.
    pub(crate) fn lower_htf_expr(
        &self,
        expr: &HirExpr,
        next_ta_slot: &mut u32,
        next_history_slot: &mut u32,
    ) -> Option<HtfExpr> {
        let recorded = self.request_default_ema_lengths.borrow().len();
        let lowered = self.lower_htf_node(expr, next_ta_slot, next_history_slot);
        if lowered.is_none() {
            self.request_default_ema_lengths
                .borrow_mut()
                .truncate(recorded);
        }
        lowered
    }

    fn lower_htf_node(
        &self,
        expr: &HirExpr,
        next_ta_slot: &mut u32,
        next_history_slot: &mut u32,
    ) -> Option<HtfExpr> {
        match &expr.kind {
            HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } => {
                let (input, index) = self.htf_enum_condition(cond)?;
                let when_equal = self.lower_htf_expr(then_e, next_ta_slot, next_history_slot)?;
                let otherwise = if matches!(else_e.kind, HirExprKind::ConstNa) {
                    None
                } else {
                    Some(Box::new(self.lower_htf_expr(
                        else_e,
                        next_ta_slot,
                        next_history_slot,
                    )?))
                };
                Some(HtfExpr::EnumSelect {
                    input,
                    index,
                    when_equal: Box::new(when_equal),
                    otherwise,
                })
            }
            HirExprKind::ConstInt(v) => Some(HtfExpr::Const(*v as f64)),
            HirExprKind::ConstFloat(v) => Some(HtfExpr::Const(*v)),
            HirExprKind::ConstBool(v) => Some(HtfExpr::Const(if *v { 1.0 } else { 0.0 })),
            HirExprKind::ReadInput { input }
                if self.inputs.get(*input as usize)?.kind == InputKind::Source =>
            {
                Some(HtfExpr::InputSource { input: *input })
            }
            HirExprKind::ReadVar { symbol, offset } => self.lower_htf_variable_history(
                expr,
                *symbol,
                *offset,
                next_ta_slot,
                next_history_slot,
            ),
            HirExprKind::ReadVarDynamic { symbol, offset } => {
                let source = self.lower_htf_variable_history(
                    expr,
                    *symbol,
                    0,
                    next_ta_slot,
                    next_history_slot,
                )?;
                self.lower_htf_history_offset(source, offset, next_history_slot)
            }
            HirExprKind::IntToFloat(inner) => {
                self.lower_htf_expr(inner, next_ta_slot, next_history_slot)
            }
            HirExprKind::ReadBuiltin {
                series: series @ (BuiltinSeries::BarstateIslast | BuiltinSeries::BarstateIsrealtime),
                offset,
            } => {
                let source = match series {
                    BuiltinSeries::BarstateIslast => HtfExpr::BarstateIslast,
                    BuiltinSeries::BarstateIsrealtime => HtfExpr::BarstateIsRealtime,
                    _ => unreachable!(),
                };
                if *offset == 0 {
                    Some(source)
                } else {
                    let history_slot = *next_history_slot;
                    *next_history_slot += 1;
                    Some(HtfExpr::History {
                        history_slot,
                        offset: *offset,
                        source: Box::new(source),
                    })
                }
            }
            HirExprKind::ReadBuiltinDynamic {
                series: series @ (BuiltinSeries::BarstateIslast | BuiltinSeries::BarstateIsrealtime),
                offset,
            } => {
                let source = match series {
                    BuiltinSeries::BarstateIslast => HtfExpr::BarstateIslast,
                    BuiltinSeries::BarstateIsrealtime => HtfExpr::BarstateIsRealtime,
                    _ => unreachable!(),
                };
                self.lower_htf_history_offset(source, offset, next_history_slot)
            }
            HirExprKind::ReadBuiltin { series, offset } if series.is_price_source() => {
                let source = HtfExpr::Source(*series);
                if *offset == 0 {
                    Some(source)
                } else {
                    let history_slot = *next_history_slot;
                    *next_history_slot += 1;
                    Some(HtfExpr::History {
                        history_slot,
                        offset: *offset,
                        source: Box::new(source),
                    })
                }
            }
            HirExprKind::ReadBuiltinDynamic { series, offset } if series.is_price_source() => {
                self.lower_htf_history_offset(HtfExpr::Source(*series), offset, next_history_slot)
            }
            HirExprKind::MaterializeHistory {
                expr: inner,
                offset,
                ..
            } => {
                let source = self.lower_htf_expr(inner, next_ta_slot, next_history_slot)?;
                let history_slot = *next_history_slot;
                *next_history_slot += 1;
                Some(HtfExpr::History {
                    history_slot,
                    offset: *offset,
                    source: Box::new(source),
                })
            }
            HirExprKind::MaterializeHistoryDynamic {
                expr: inner,
                offset,
                ..
            } => {
                let source = self.lower_htf_expr(inner, next_ta_slot, next_history_slot)?;
                self.lower_htf_history_offset(source, offset, next_history_slot)
            }
            HirExprKind::Unary {
                op: UnOp::Neg,
                operand,
            } => Some(HtfExpr::Neg(Box::new(self.lower_htf_expr(
                operand,
                next_ta_slot,
                next_history_slot,
            )?))),
            HirExprKind::Binary { op, lhs, rhs }
                if matches!(
                    op,
                    BinOp::Add
                        | BinOp::Sub
                        | BinOp::Mul
                        | BinOp::Div
                        | BinOp::Rem
                        | BinOp::Lt
                        | BinOp::Le
                        | BinOp::Gt
                        | BinOp::Ge
                        | BinOp::Eq
                        | BinOp::Ne
                ) =>
            {
                let lhs = self.lower_htf_expr(lhs, next_ta_slot, next_history_slot)?;
                let rhs = self.lower_htf_expr(rhs, next_ta_slot, next_history_slot)?;
                // An integer-typed node (`int OP int -> int`) needs integer
                // semantics for `%` in the float-only secondary context;
                // division still retains its fraction. `Type::Int` is visible
                // on this Binary node even when it feeds a float context
                // through an `IntToFloat` wrapper (which recurses in here,
                // preserving this node's type).
                Some(HtfExpr::Binary {
                    op: *op,
                    int_result: expr.ty == Type::Int,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                })
            }
            // Slot-based ta.* (ta.sma/ta.change): the source lowers, the site
            // takes its own secondary-context state slot.
            HirExprKind::TaCall {
                func,
                slot,
                source,
                length,
            } if matches!(func, TaFunc::Sma | TaFunc::Change) => {
                let binding = self.ta_input_bindings.iter().find(|binding| matches!(binding.target,
                    pinewasm_hir::TaInputTarget::Sma { slot: target } | pinewasm_hir::TaInputTarget::Change { slot: target } if target == slot.0));
                if binding.is_some_and(|binding| binding.negated) {
                    return None;
                }
                let src = self.lower_htf_expr(source, next_ta_slot, next_history_slot)?;
                let ta_slot = *next_ta_slot;
                *next_ta_slot += 1;
                Some(if let Some(binding) = binding {
                    HtfExpr::TaInput {
                        func: *func,
                        ta_slot,
                        input: binding.input,
                        binding_target: binding.target,
                        default_length: *length,
                        source: Box::new(src),
                    }
                } else {
                    HtfExpr::Ta {
                        func: *func,
                        ta_slot,
                        length: *length,
                        source: Box::new(src),
                    }
                })
            }
            HirExprKind::CallTaState {
                func: TaFunc::Atr,
                state,
                args,
            } if args.is_empty() => {
                let length = *self.ta_states.get(*state as usize)?.params.first()?;
                let length = u32::try_from(length)
                    .ok()
                    .filter(|length| (1..=100_000).contains(length))?;
                let ta_slot = *next_ta_slot;
                *next_ta_slot += 1;
                Some(HtfExpr::Ta {
                    func: TaFunc::Atr,
                    ta_slot,
                    length,
                    // ATR reads the requested context's high/low/previous
                    // close lanes directly through the TA engine.
                    source: Box::new(HtfExpr::Const(0.0)),
                })
            }
            HirExprKind::CallTaState {
                func: func @ (TaFunc::Ema | TaFunc::Hma | TaFunc::Vwma | TaFunc::Rsi | TaFunc::Wma),
                state,
                args,
            } if args.len() == 1 => {
                let binding = self.ta_input_bindings.iter().find(|binding| matches!(binding.target,
                    pinewasm_hir::TaInputTarget::State { state: target, param: 0 } if target == *state));
                if binding.is_some_and(|binding| binding.negated) {
                    return None;
                }
                let length = *self.ta_states.get(*state as usize)?.params.first()?;
                let length = u32::try_from(length).ok().filter(|length| {
                    (*func != TaFunc::Hma || *length >= 2) && (1..=100_000).contains(length)
                })?;
                let src = self.lower_htf_expr(&args[0], next_ta_slot, next_history_slot)?;
                let ta_slot = *next_ta_slot;
                *next_ta_slot += 1;
                Some(if let Some(binding) = binding {
                    HtfExpr::TaInput {
                        func: *func,
                        ta_slot,
                        input: binding.input,
                        binding_target: binding.target,
                        default_length: length,
                        source: Box::new(src),
                    }
                } else {
                    HtfExpr::Ta {
                        func: *func,
                        ta_slot,
                        length,
                        source: Box::new(src),
                    }
                })
            }
            // An EMA length computed from inputs is a run-time length on the
            // chart. The secondary context has none, so it keeps the length
            // the inputs' defaults give, as it did while that length was
            // folded at compile time. Any other run-time length is `E0244`.
            HirExprKind::TaCallDynamic {
                func: TaFunc::Ema,
                slot,
                source,
                length: length_expr,
            } => {
                let default_length = *self.input_default_ema_lengths.get(slot)?;
                let length = u32::try_from(default_length)
                    .ok()
                    .filter(|length| (1..=100_000).contains(length))?;
                let src = self.lower_htf_expr(source, next_ta_slot, next_history_slot)?;
                self.request_default_ema_lengths
                    .borrow_mut()
                    .push((length_expr.span, default_length));
                let ta_slot = *next_ta_slot;
                *next_ta_slot += 1;
                Some(HtfExpr::Ta {
                    func: TaFunc::Ema,
                    ta_slot,
                    length,
                    source: Box::new(src),
                })
            }
            // Other engine-backed ta.* remain E0244.
            _ => None,
        }
    }

    fn htf_enum_condition(&self, expr: &HirExpr) -> Option<(u32, u32)> {
        let HirExprKind::Binary {
            op: BinOp::Eq,
            lhs,
            rhs,
        } = &expr.kind
        else {
            return None;
        };
        let Type::Enum(name) = &lhs.ty else {
            return None;
        };
        if lhs.ty != rhs.ty {
            return None;
        }
        let (value, label) = if matches!(rhs.kind, HirExprKind::ConstInt(_)) {
            (lhs, rhs)
        } else {
            (rhs, lhs)
        };
        let HirExprKind::ConstInt(index) = label.kind else {
            return None;
        };
        let input = self.selected_input_for_expr(value)?;
        let decl = self.inputs.get(input as usize)?;
        if decl.kind != InputKind::Enum
            || !matches!(&decl.defval, InputValue::Enum { name: actual, .. } if actual == name)
        {
            return None;
        }
        Some((input, u32::try_from(index).ok()?))
    }

    /// Reconstruct an immutable variable's value in the requested context
    /// before applying history. A chart-side parameter slot has a different
    /// time axis and must never supply the historical requested value.
    fn lower_htf_variable_history(
        &self,
        expr: &HirExpr,
        symbol: SymbolId,
        offset: u32,
        next_ta_slot: &mut u32,
        next_history_slot: &mut u32,
    ) -> Option<HtfExpr> {
        let declaration = self.symbols.get(symbol.0 as usize)?;
        if declaration.persistent || declaration.varip {
            return None;
        }
        let mut current = expr.clone();
        current.kind = HirExprKind::ReadVar { symbol, offset: 0 };
        let input_source = self.input_origin_for_expr(&current).and_then(|origin| {
            (self.inputs.get(origin.input as usize)?.kind == InputKind::Source && !origin.inverted)
                .then_some(HtfExpr::InputSource {
                    input: origin.input,
                })
        });
        let source = match input_source {
            Some(source) => source,
            None => self.lower_htf_expr(
                self.htf_expr_origins.get(&symbol)?,
                next_ta_slot,
                next_history_slot,
            )?,
        };
        if offset == 0 {
            return Some(source);
        }
        let history_slot = *next_history_slot;
        *next_history_slot += 1;
        Some(HtfExpr::History {
            history_slot,
            offset,
            source: Box::new(source),
        })
    }

    fn lower_htf_history_offset(
        &self,
        source: HtfExpr,
        offset: &HirExpr,
        next_history_slot: &mut u32,
    ) -> Option<HtfExpr> {
        if let HirExprKind::Ternary {
            cond,
            then_e,
            else_e,
        } = &offset.kind
        {
            if let (Some(condition), Some(true_offset), Some(false_offset)) = (
                self.htf_realtime_condition(cond, 0),
                Self::htf_constant_history_offset(then_e),
                Self::htf_constant_history_offset(else_e),
            ) {
                let history_slot = *next_history_slot;
                *next_history_slot = next_history_slot.checked_add(1)?;
                return match condition {
                    RealtimeCondition::Realtime => Some(HtfExpr::HistoryRealtime {
                        history_slot,
                        history_offset: false_offset,
                        realtime_offset: true_offset,
                        source: Box::new(source),
                    }),
                    RealtimeCondition::Historical => Some(HtfExpr::HistoryRealtime {
                        history_slot,
                        history_offset: true_offset,
                        realtime_offset: false_offset,
                        source: Box::new(source),
                    }),
                    RealtimeCondition::Always(true) => Some(HtfExpr::History {
                        history_slot,
                        offset: true_offset,
                        source: Box::new(source),
                    }),
                    RealtimeCondition::Always(false) => Some(HtfExpr::History {
                        history_slot,
                        offset: false_offset,
                        source: Box::new(source),
                    }),
                };
            }
        }
        let offset = self.lower_htf_offset_expr(offset)?;
        let history_slot = *next_history_slot;
        let history = if !offset.uses_input() {
            let value = offset.evaluate(&|_| None)?;
            let offset = match value {
                InputValue::Int(x) => u32::try_from(x).ok()?,
                InputValue::Float(x) if x.trunc() >= 0.0 && x.trunc() <= f64::from(u32::MAX) => {
                    x.trunc() as u32
                }
                _ => return None,
            };
            HtfExpr::History {
                history_slot,
                offset,
                source: Box::new(source),
            }
        } else {
            HtfExpr::HistoryInput {
                history_slot,
                offset,
                source: Box::new(source),
            }
        };
        *next_history_slot += 1;
        Some(history)
    }

    fn htf_constant_history_offset(expr: &HirExpr) -> Option<u32> {
        match &expr.kind {
            HirExprKind::ConstInt(value) => u32::try_from(*value).ok(),
            HirExprKind::ConstFloat(value)
                if value.is_finite()
                    && value.trunc() >= 0.0
                    && value.trunc() <= f64::from(u32::MAX) =>
            {
                Some(value.trunc() as u32)
            }
            HirExprKind::IntToFloat(inner) => Self::htf_constant_history_offset(inner),
            _ => None,
        }
    }

    /// Classify the boolean subset needed by history selectors that depend on
    /// realtime conditions. Function parameters are traced through their
    /// call-site HIR origins, so `not flag and barstate.isrealtime` specializes
    /// correctly when `flag` is a literal.
    fn htf_realtime_condition(&self, expr: &HirExpr, depth: usize) -> Option<RealtimeCondition> {
        if depth > 64 {
            return None;
        }
        match &expr.kind {
            HirExprKind::ConstBool(value) => Some(RealtimeCondition::Always(*value)),
            HirExprKind::ReadBuiltin {
                series: BuiltinSeries::BarstateIsrealtime,
                offset: 0,
            } => Some(RealtimeCondition::Realtime),
            HirExprKind::ReadVar { symbol, offset: 0 }
                if self
                    .symbols
                    .get(symbol.0 as usize)
                    .is_some_and(|declaration| !declaration.persistent && !declaration.varip) =>
            {
                match self.const_input_values.get(symbol) {
                    Some(InputValue::Bool(value)) => {
                        self.defer_temporal_fixedness_at(
                            expr,
                            "request.security",
                            "expression",
                            expr.span,
                        );
                        Some(RealtimeCondition::Always(*value))
                    }
                    _ => self.htf_realtime_condition(self.htf_expr_origins.get(symbol)?, depth + 1),
                }
            }
            HirExprKind::Unary {
                op: UnOp::Not,
                operand,
            } => Some(self.htf_realtime_condition(operand, depth + 1)?.negate()),
            HirExprKind::Binary {
                op: BinOp::And,
                lhs,
                rhs,
            } => Some(
                self.htf_realtime_condition(lhs, depth + 1)?
                    .and(self.htf_realtime_condition(rhs, depth + 1)?),
            ),
            HirExprKind::Binary {
                op: BinOp::Or,
                lhs,
                rhs,
            } => Some(
                self.htf_realtime_condition(lhs, depth + 1)?
                    .or(self.htf_realtime_condition(rhs, depth + 1)?),
            ),
            HirExprKind::Ternary {
                cond,
                then_e,
                else_e,
            } => match self.htf_realtime_condition(cond, depth + 1)? {
                RealtimeCondition::Always(true) => self.htf_realtime_condition(then_e, depth + 1),
                RealtimeCondition::Always(false) => self.htf_realtime_condition(else_e, depth + 1),
                _ => None,
            },
            _ => None,
        }
    }

    fn lower_htf_offset_expr(&self, expr: &HirExpr) -> Option<pinewasm_hir::HtfHistoryOffset> {
        use pinewasm_hir::HtfHistoryOffset as O;
        if let Some(origin) = self.input_origin_for_expr(expr) {
            if origin.inverted {
                return None;
            }
            let decl = self.inputs.get(origin.input as usize)?;
            let offset = match decl.defval {
                InputValue::Int(default) if decl.kind == InputKind::Int => Some(O::InputInt {
                    input: origin.input,
                    default,
                }),
                InputValue::Float(default) if decl.kind == InputKind::Float => {
                    Some(O::InputFloat {
                        input: origin.input,
                        default,
                    })
                }
                _ => None,
            };
            if offset.is_some() {
                self.defer_temporal_fixedness_at(expr, "request.security", "expression", expr.span);
            }
            return offset;
        }
        match &expr.kind {
            HirExprKind::ConstInt(x) => Some(O::Int(*x)),
            HirExprKind::ConstFloat(x) => Some(O::Float(*x)),
            HirExprKind::ReadVar { symbol, offset: 0 }
                if self
                    .symbols
                    .get(symbol.0 as usize)
                    .is_some_and(|declaration| !declaration.persistent && !declaration.varip) =>
            {
                self.lower_htf_offset_expr(self.htf_expr_origins.get(symbol)?)
            }
            HirExprKind::IntToFloat(x) => {
                Some(O::CastFloat(Box::new(self.lower_htf_offset_expr(x)?)))
            }
            HirExprKind::Unary {
                op: UnOp::Neg,
                operand,
            } => Some(O::Neg(Box::new(self.lower_htf_offset_expr(operand)?))),
            HirExprKind::CallPure { func, args }
                if matches!(func, PureFunc::CastInt | PureFunc::CastFloat) =>
            {
                let value = Box::new(self.lower_htf_offset_expr(args.first()?)?);
                Some(if *func == PureFunc::CastInt {
                    O::CastInt(value)
                } else {
                    O::CastFloat(value)
                })
            }
            HirExprKind::Binary { op, lhs, rhs }
                if matches!(
                    op,
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem
                ) =>
            {
                Some(O::Binary {
                    op: *op,
                    lhs: Box::new(self.lower_htf_offset_expr(lhs)?),
                    rhs: Box::new(self.lower_htf_offset_expr(rhs)?),
                })
            }
            _ => None,
        }
    }

    /// Resolve an optional `barmerge.*` argument to its on/off flag. `None`
    /// (argument omitted) is the `off` default. A supplied argument must be the
    /// exact matching `off_token`/`on_token`; anything else is `E0242`.
    pub(crate) fn security_barmerge_flag(
        &mut self,
        arg: Option<&ast::Expr>,
        param: &str,
        off_token: &str,
        on_token: &str,
    ) -> Result<bool, ()> {
        let Some(arg) = arg else {
            return Ok(false); // omitted → default off
        };
        let hir = self.analyze_expr(arg);
        if hir.ty.is_error() {
            return Err(());
        }
        self.defer_temporal_fixedness_at(&hir, "request.security", param, arg.span());
        match &hir.kind {
            HirExprKind::ConstString(s) if s == off_token => Ok(false),
            HirExprKind::ConstString(s) if s == on_token => Ok(true),
            _ => {
                self.error(
                    codes::E0242,
                    format!("`request.security` `{param}` must be `{off_token}` or `{on_token}`"),
                    arg.span(),
                    format!("pass `{off_token}` or `{on_token}`"),
                );
                Err(())
            }
        }
    }

    /// Check a scalar `request.*` external-data interface call.
    /// Each function uses fixed parameter names, arity and scalar types and
    /// produces a `series float`.
    ///
    /// This network-free VM cannot fetch fundamentals, corporate actions, macro
    /// series, or fx rates, so each is modeled as an INTERFACE: the language
    /// surface (signature, typing) is real and the runtime reads the value from
    /// an optional per-run host table (default `na`; see docs/runtime.md). The
    /// analyzer therefore:
    ///  - requires the leading *identifying* arguments (ticker/symbol/field/
    ///    `financial_id/period/country/currency`) to be constant string literals —
    ///    they are folded into a stable selector key that the VM looks up in the
    ///    host table (a non-constant identifier is `E0242`, `request.*`);
    ///  - binds positional and named arguments against the complete per-function
    ///    signature and type-checks optional arguments (`gaps`, `lookahead`,
    ///    invalid-symbol flags, currency, and history caps); these parameters
    ///    remain host concerns because the interface returns a supplied value or
    ///    `na` without fetching or transforming data itself.
    ///
    /// Check a `request.security_lower_tf(symbol, timeframe, expression,
    /// ignore_invalid_symbol?, currency?, ignore_invalid_timeframe?,
    /// calc_bars_count?)` call. Returns an `array<float>` of the intrabar values within
    /// each chart bar, read from the optional host table (default empty). Like
    /// `request.security` the `symbol` must be the chart's own feed
    /// (`syminfo.ticker`/`tickerid`) and the `timeframe` a constant string;
    /// both, together with the `expression`, are folded into a stable selector
    /// key so distinct calls on the same symbol/timeframe but different
    /// expressions (e.g. `close` vs `volume`) address distinct host-table
    /// entries rather than silently colliding. `expression` must therefore be
    /// a plain OHLCV/derived builtin series read (mirroring `request.security`'s
    /// plain-expression form); its value still comes from the host table (this
    /// VM only aggregates UP, so lower-timeframe intrabar decomposition is a
    /// host concern). Diagnostics use `E0246`.
    pub(crate) fn analyze_security_lower_tf_call(
        &mut self,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let member = "security_lower_tf";
        let (bound, overflow) = match self.bind_call_args(
            "request.security_lower_tf",
            &SECURITY_LOWER_TF_PARAM_NAMES,
            args,
        ) {
            Some(bound) => bound,
            None => return self.error_expr(span),
        };
        // symbol, timeframe, expression are required; the remaining
        // parameters are type-checked host controls.
        if bound[..3].iter().any(Option::is_none) || !overflow.is_empty() {
            self.error(
                codes::E0204,
                format!(
                    "`request.{member}` expects 3 to 7 argument(s), found {}",
                    args.len()
                ),
                span,
                "expected symbol, timeframe, expression, and at most four optional host controls",
            );
            for arg in bound.iter().flatten().chain(overflow.iter()) {
                let _ = self.analyze_expr(arg);
            }
            return self.error_expr(span);
        }
        let symbol_arg = bound[0].expect("required argument checked above");
        let timeframe_arg = bound[1].expect("required argument checked above");
        let expression_arg = bound[2].expect("required argument checked above");

        let mut ok = true;
        // --- symbol: must name the chart's own feed --------------------------
        let symbol_hir = {
            let symbol = self.analyze_expr(symbol_arg);
            self.chart_symbol_for_empty(symbol)
        };
        self.defer_temporal_fixedness_at(
            &symbol_hir,
            "request.security_lower_tf",
            "symbol",
            symbol_arg.span(),
        );
        let symbol = match self.current_feed_symbol_for_expr(&symbol_hir) {
            Some(series) => series.name().to_string(),
            None => {
                if !symbol_hir.ty.is_error() {
                    self.error(
                        codes::E0246,
                        format!(
                            "`request.{member}` symbol must be `syminfo.tickerid` (the chart's own feed)"
                        ),
                        symbol_arg.span(),
                        "this interface reads the chart feed's intrabar values from an optional host table",
                    );
                }
                ok = false;
                String::new()
            }
        };
        // --- timeframe: a constant string ------------------------------------
        let tf_hir = self.analyze_expr(timeframe_arg);
        self.defer_temporal_fixedness_at(
            &tf_hir,
            "request.security_lower_tf",
            "timeframe",
            timeframe_arg.span(),
        );
        let timeframe_value = match &tf_hir.kind {
            HirExprKind::ConstString(tf) => Some(tf.clone()),
            _ => self
                .const_input_value_from_hir(&tf_hir)
                .and_then(|value| match value {
                    InputValue::String(value) => Some(value),
                    _ => None,
                })
                .or_else(|| {
                    let origin = self.input_origin_for_expr(&tf_hir)?;
                    match &self.inputs.get(origin.input as usize)?.defval {
                        InputValue::String(value) => Some(value.clone()),
                        _ => None,
                    }
                }),
        };
        let timeframe = match timeframe_value {
            Some(tf) => tf,
            None => {
                if !tf_hir.ty.is_error() && tf_hir.ty != Type::String {
                    // A non-string timeframe is a type error, not the
                    // order-flow restriction `E0246` reports.
                    self.error(
                        codes::E0205,
                        format!(
                            "`request.{member}` argument `timeframe` must be string, found {}",
                            tf_hir.ty.describe()
                        ),
                        timeframe_arg.span(),
                        "expected string",
                    );
                } else if !tf_hir.ty.is_error() {
                    self.error(
                        codes::E0246,
                        format!("`request.{member}` timeframe must be a constant string literal"),
                        timeframe_arg.span(),
                        "pass a lower-timeframe string literal (e.g. \"1\", \"5\")",
                    );
                }
                ok = false;
                String::new()
            }
        };
        // --- expression: a plain price series, folded into the selector so a
        // second call on the same symbol/timeframe with a different
        // expression addresses a distinct host-table entry instead of
        // silently colliding with this one.
        let expr_hir = self.analyze_expr(expression_arg);
        let mut source = BuiltinSeries::Close;
        match &expr_hir.kind {
            HirExprKind::ReadBuiltin { series, offset: 0 } if series.is_price_source() => {
                source = *series;
            }
            _ => {
                if !expr_hir.ty.is_error() {
                    self.error(
                        codes::E0246,
                        format!("`request.{member}` expression must be a plain price series"),
                        expression_arg.span(),
                        "pass a plain OHLCV/derived series like `close`, `hl2`, or `volume`",
                    );
                }
                ok = false;
            }
        }
        // Optional host controls bind by their canonical names and are
        // type-checked even though this network-free interface cannot apply
        // symbol/timeframe suppression, currency conversion, or feed truncation.
        for (index, expected) in [
            (3usize, Type::Bool),
            (4usize, Type::String),
            (5usize, Type::Bool),
            (6usize, Type::Int),
        ] {
            let Some(arg) = bound[index] else { continue };
            let hir = self.analyze_expr_expecting(arg, Some(&expected));
            if !hir.ty.is_error() && hir.ty != expected {
                self.error(
                    codes::E0205,
                    format!(
                        "`request.{member}` argument `{}` must be {}, found {}",
                        SECURITY_LOWER_TF_PARAM_NAMES[index],
                        expected.describe(),
                        hir.ty.describe()
                    ),
                    arg.span(),
                    format!("expected {}", expected.describe()),
                );
                ok = false;
            } else if hir.ty.is_error() {
                ok = false;
            }
        }

        if !ok {
            return self.error_expr(span);
        }
        let selector = format!("security_lower_tf/{symbol}/{timeframe}/{source:?}");
        HirExpr {
            kind: HirExprKind::ReadSecurityLowerTf { selector },
            ty: Type::Array(Box::new(Type::Float)),
            qualifier: Qualifier::Series,
            span,
        }
    }

    /// Check the operands of `ta.pivot_point_levels`.
    /// All three operands are series-capable; `developing` defaults to false.
    pub(crate) fn analyze_pivot_point_levels_call(
        &mut self,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let (bound, overflow) = match self.bind_call_args(
            "ta.pivot_point_levels",
            &PIVOT_POINT_LEVELS_PARAM_NAMES,
            args,
        ) {
            Some(a) => a,
            None => return self.error_expr(span),
        };
        // `type` and `anchor` are required. `developing` is optional.
        if bound[0].is_none() || bound[1].is_none() || !overflow.is_empty() {
            self.error(
                codes::E0204,
                format!(
                    "`ta.pivot_point_levels` expects 2 to 3 argument(s), found {}",
                    args.len()
                ),
                span,
                "expected 2 to 3 argument(s) (type, anchor, developing?)",
            );
            for arg in bound.iter().flatten().chain(overflow.iter()) {
                let _ = self.analyze_expr(arg);
            }
            return self.error_expr(span);
        }
        let type_arg = bound[0].unwrap();
        let type_hir = self.analyze_expr_expecting(type_arg, Some(&Type::String));
        let anchor_arg = bound[1].expect("required argument checked above");
        let anchor_hir = self.analyze_expr_expecting(anchor_arg, Some(&Type::Bool));
        let developing_hir = if let Some(arg) = bound[2] {
            self.analyze_expr_expecting(arg, Some(&Type::Bool))
        } else {
            HirExpr {
                kind: HirExprKind::ConstBool(false),
                ty: Type::Bool,
                qualifier: Qualifier::Const,
                span,
            }
        };
        let mut ok = true;
        for (arg, hir, name, expected) in [
            (type_arg, &type_hir, "type", "string"),
            (anchor_arg, &anchor_hir, "anchor", "bool"),
        ] {
            let valid = match expected {
                "string" => hir.ty == Type::String,
                "bool" => hir.ty == Type::Bool,
                _ => false,
            };
            if !hir.ty.is_error() && !valid {
                self.error(
                    codes::E0205,
                    format!(
                        "`ta.pivot_point_levels` `{name}` must be {expected}, found {}",
                        hir.ty.describe()
                    ),
                    arg.span(),
                    format!("expected a series {expected}"),
                );
                ok = false;
            } else if hir.ty.is_error() {
                ok = false;
            }
        }
        if let Some(arg) = bound[2] {
            if !developing_hir.ty.is_error() && developing_hir.ty != Type::Bool {
                self.error(
                    codes::E0205,
                    format!(
                        "`ta.pivot_point_levels` `developing` must be bool, found {}",
                        developing_hir.ty.describe()
                    ),
                    arg.span(),
                    "expected a series bool",
                );
                ok = false;
            }
        }
        if developing_hir.ty.is_error() {
            ok = false;
        }
        if !ok {
            return self.error_expr(span);
        }
        HirExpr {
            kind: HirExprKind::CallPivotPointLevels {
                args: vec![type_hir, anchor_hir, developing_hir],
            },
            ty: Type::Array(Box::new(Type::Float)),
            qualifier: Qualifier::Series,
            span,
        }
    }

    /// Analyze the value-producing `time()` / `time_close()` function forms.
    ///
    /// The runtime call shape is fixed:
    /// `time(timeframe, session, timezone, bars_back, timeframe_bars_back)` and
    /// `time_close(timeframe, session, timezone, bars_back, timeframe_bars_back)`.
    /// Missing optional operands are materialized as `""`, `""`, `0`, and `0`.
    pub(crate) fn analyze_time_function_call(
        &mut self,
        name: &'static str,
        pure: PureFunc,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        if args.is_empty() || args.len() > TIME_FUNCTION_PARAM_NAMES.len() {
            self.error(
                codes::E0204,
                format!("`{name}` expects 1 to 5 argument(s), found {}", args.len()),
                span,
                "expected timeframe, with optional session/timezone/offset arguments",
            );
            for arg in args {
                let _ = self.analyze_expr(&arg.value);
            }
            return self.error_expr(span);
        }

        let mut session: Option<(HirExpr, Span)> = None;
        let mut timezone: Option<(HirExpr, Span)> = None;
        let mut bars_back: Option<(HirExpr, Span)> = None;
        let mut timeframe_bars_back: Option<(HirExpr, Span)> = None;
        // Assigned on both binding paths, before the optional operands.
        let timeframe: HirExpr;

        if args.iter().any(|a| a.name.is_some()) {
            let Some((bound, overflow)) =
                self.bind_call_args(name, &TIME_FUNCTION_PARAM_NAMES, args)
            else {
                return self.error_expr(span);
            };
            if !overflow.is_empty() || bound[0].is_none() {
                self.error(
                    codes::E0204,
                    format!("`{name}` expects 1 to 5 argument(s), found {}", args.len()),
                    span,
                    "the `timeframe` argument is required",
                );
                for arg in bound.iter().flatten().chain(overflow.iter()) {
                    let _ = self.analyze_expr(arg);
                }
                return self.error_expr(span);
            }
            // Parameter order: the timeframe runs before the optional
            // operands, so a user function in a later one runs after it.
            timeframe =
                self.analyze_time_string_arg(name, "timeframe", bound[0].expect("checked above"));
            if let Some(arg) = bound[1] {
                session = Some((
                    self.analyze_time_string_arg(name, "session", arg),
                    arg.span(),
                ));
            }
            if let Some(arg) = bound[2] {
                timezone = Some((
                    self.analyze_time_string_arg(name, "timezone", arg),
                    arg.span(),
                ));
            }
            if let Some(arg) = bound[3] {
                bars_back = Some((
                    self.analyze_time_int_arg(name, "bars_back", arg),
                    arg.span(),
                ));
            }
            if let Some(arg) = bound[4] {
                timeframe_bars_back = Some((
                    self.analyze_time_int_arg(name, "timeframe_bars_back", arg),
                    arg.span(),
                ));
            }
        } else {
            timeframe = self.analyze_time_string_arg(name, "timeframe", &args[0].value);
            let mut next_string_slot = 0usize;
            let mut next_int_slot = 0usize;
            for arg in args.iter().skip(1) {
                let hir = self.analyze_expr(&arg.value);
                let hir_ty = hir.ty.clone();
                match hir_ty {
                    Type::String => {
                        if next_string_slot == 0 {
                            session = Some((hir, arg.value.span()));
                        } else if next_string_slot == 1 {
                            timezone = Some((hir, arg.value.span()));
                        } else {
                            self.error(
                                codes::E0204,
                                format!("`{name}` received too many string arguments"),
                                arg.value.span(),
                                "only session and timezone can be strings",
                            );
                        }
                        next_string_slot += 1;
                    }
                    Type::Int => {
                        if next_int_slot == 0 {
                            bars_back = Some((hir, arg.value.span()));
                        } else if next_int_slot == 1 {
                            timeframe_bars_back = Some((hir, arg.value.span()));
                        } else {
                            self.error(
                                codes::E0204,
                                format!("`{name}` received too many offset arguments"),
                                arg.value.span(),
                                "only bars_back and timeframe_bars_back can be offsets",
                            );
                        }
                        next_int_slot += 1;
                    }
                    Type::Error => {}
                    other => {
                        self.error(
                            codes::E0205,
                            format!(
                                "`{name}` optional arguments must be string sessions/timezones or int offsets, found {}",
                                other.describe()
                            ),
                            arg.value.span(),
                            "expected a string or int optional argument",
                        );
                    }
                }
            }
        }

        if timeframe.ty.is_error()
            || session.as_ref().is_some_and(|(h, _)| h.ty.is_error())
            || timezone.as_ref().is_some_and(|(h, _)| h.ty.is_error())
            || bars_back.as_ref().is_some_and(|(h, _)| h.ty.is_error())
            || timeframe_bars_back
                .as_ref()
                .is_some_and(|(h, _)| h.ty.is_error())
        {
            return self.error_expr(span);
        }

        let session = session.map_or_else(|| Self::const_string_expr("", span), |(arg, _)| arg);
        let timezone = timezone.map_or_else(|| Self::const_string_expr("", span), |(arg, _)| arg);
        let bars_back = bars_back.map_or_else(|| Self::const_int_expr(0, span), |(arg, _)| arg);
        let timeframe_bars_back =
            timeframe_bars_back.map_or_else(|| Self::const_int_expr(0, span), |(arg, _)| arg);

        HirExpr {
            kind: HirExprKind::CallPure {
                func: pure,
                args: vec![timeframe, session, timezone, bars_back, timeframe_bars_back],
            },
            ty: Type::Int,
            qualifier: Qualifier::Series,
            span,
        }
    }

    pub(crate) fn analyze_time_string_arg(
        &mut self,
        name: &str,
        param: &str,
        arg: &ast::Expr,
    ) -> HirExpr {
        let hir = self.analyze_expr_expecting(arg, Some(&Type::String));
        if !hir.ty.is_error() && hir.ty != Type::String {
            self.error(
                codes::E0205,
                format!(
                    "`{name}` argument `{param}` must be string, found {}",
                    hir.ty.describe()
                ),
                arg.span(),
                "expected a string value",
            );
        }
        hir
    }

    pub(crate) fn analyze_time_int_arg(
        &mut self,
        name: &str,
        param: &str,
        arg: &ast::Expr,
    ) -> HirExpr {
        let hir = self.analyze_expr_expecting(arg, Some(&Type::Int));
        if !hir.ty.is_error() && hir.ty != Type::Int {
            self.error(
                codes::E0205,
                format!(
                    "`{name}` argument `{param}` must be int, found {}",
                    hir.ty.describe()
                ),
                arg.span(),
                "expected an int value",
            );
        }
        hir
    }

    /// Parameter names and arity are fixed per function in this compiler's
    /// signature table. Identifying strings fold into deterministic host-table keys;
    /// the chart symbol in corporate-action and financial requests is resolved
    /// against the current feed at run time.
    pub(crate) fn analyze_request_call(
        &mut self,
        func: RequestFunc,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let param_kinds = request_param_signature(func);
        let member = func.member();
        let callee = format!("request.{member}");
        let param_names = func.param_names();
        let required = func.required_count();
        assert_eq!(param_names.len(), param_kinds.len());
        let (bound, overflow) = match self.bind_call_args(&callee, param_names, args) {
            Some(bound) => bound,
            None => return self.error_expr(span),
        };

        let missing_required = bound[..required].iter().any(Option::is_none);
        if missing_required || !overflow.is_empty() {
            self.error(
                codes::E0204,
                format!(
                    "`request.{member}` expects {} to {} argument(s), found {}",
                    required,
                    param_names.len(),
                    args.len()
                ),
                span,
                format!(
                    "expected {} required argument(s) ({}) and at most {} total",
                    required,
                    param_names[..required].join(", "),
                    param_names.len()
                ),
            );
            for arg in bound.iter().flatten().chain(overflow.iter()) {
                let _ = self.analyze_expr(arg);
            }
            return self.error_expr(span);
        }

        let mut ok = true;
        let mut selector = String::from(member);
        for (idx, ((param, kind), arg)) in param_names
            .iter()
            .zip(param_kinds.iter())
            .zip(bound.iter())
            .enumerate()
        {
            let Some(arg) = arg else {
                // These two fields have fixed default values. Include the
                // resolved value in the host selector so the shorthand call
                // cannot collide with a different field.
                let default_selector = match (func, idx) {
                    (RequestFunc::Dividends, 1) => Some("dividends.gross"),
                    (RequestFunc::Earnings, 1) => Some("earnings.actual"),
                    _ => None,
                };
                if let Some(value) = default_selector {
                    selector.push('/');
                    selector.push_str(value);
                }
                continue;
            };
            let hir = self.analyze_expr_expecting(
                arg,
                match kind {
                    RequestParamKind::String | RequestParamKind::Selector => Some(&Type::String),
                    RequestParamKind::Bool => Some(&Type::Bool),
                    RequestParamKind::Int => Some(&Type::Int),
                    RequestParamKind::Numeric => Some(&Type::Float),
                },
            );
            if hir.ty.is_error() {
                ok = false;
                continue;
            }

            if matches!(kind, RequestParamKind::Selector) {
                if func != RequestFunc::Quandl {
                    self.defer_temporal_fixedness_at(&hir, &callee, param, arg.span());
                }
                if let HirExprKind::ConstString(value) = &hir.kind {
                    selector.push('/');
                    selector.push_str(value);
                } else if idx == 0
                    && matches!(
                        func,
                        RequestFunc::Dividends
                            | RequestFunc::Splits
                            | RequestFunc::Earnings
                            | RequestFunc::Financial
                    )
                    && self.current_feed_symbol_for_expr(&hir).is_some()
                {
                    let series = self.current_feed_symbol_for_expr(&hir).unwrap();
                    selector.push_str(match series {
                        BuiltinSeries::SyminfoTicker => "_chart_ticker",
                        BuiltinSeries::SyminfoTickerid => "_chart_tickerid",
                        _ => unreachable!("chart symbol provenance has only two builtins"),
                    });
                } else {
                    self.error(
                        codes::E0242,
                        format!("`request.{member}` `{param}` must be a constant string literal"),
                        arg.span(),
                        "pass a string literal (this interface reads from an optional host table)",
                    );
                    ok = false;
                }
                continue;
            }

            let valid = match kind {
                RequestParamKind::Selector => unreachable!("selector positions handled above"),
                RequestParamKind::String => hir.ty == Type::String,
                RequestParamKind::Bool => hir.ty == Type::Bool,
                RequestParamKind::Int => hir.ty == Type::Int,
                RequestParamKind::Numeric => matches!(hir.ty, Type::Int | Type::Float),
            };
            if !valid {
                let expected = match kind {
                    RequestParamKind::Selector | RequestParamKind::String => "string",
                    RequestParamKind::Bool => "bool",
                    RequestParamKind::Int => "int",
                    RequestParamKind::Numeric => "int or float series",
                };
                self.error(
                    codes::E0205,
                    format!(
                        "`request.{member}` argument `{param}` must be {expected}, found {}",
                        hir.ty.describe()
                    ),
                    arg.span(),
                    format!("expected {expected}"),
                );
                ok = false;
            }
        }

        if !ok {
            return self.error_expr(span);
        }
        // `request.quandl` is deprecated. Its argument surface remains
        // type-checked, but it never reaches a host table and deterministically
        // yields `na`.
        if func == RequestFunc::Quandl {
            return HirExpr {
                kind: HirExprKind::ConstNa,
                ty: Type::Float,
                qualifier: Qualifier::Series,
                span,
            };
        }
        // Equal from and to currencies yield 1. Both currencies are constant
        // strings here, so the rate needs no host data.
        if func == RequestFunc::CurrencyRate {
            let mut parts = selector.split('/').skip(1);
            if let (Some(from), Some(to)) = (parts.next(), parts.next()) {
                if from == to {
                    return HirExpr {
                        kind: HirExprKind::ConstFloat(1.0),
                        ty: Type::Float,
                        qualifier: Qualifier::Series,
                        span,
                    };
                }
            }
        }
        HirExpr {
            kind: HirExprKind::ReadRequest { func, selector },
            ty: Type::Float,
            qualifier: Qualifier::Series,
            span,
        }
    }
}
