//! Script declarations and statement analysis: `indicator()`/`strategy()`/
//! `library()` declarations, user function/method/type/enum collection,
//! variable declarations and (re)assignment, tuple destructuring, expression
//! statements, and the `if`/`switch`/`for`/`while` statement forms.

use crate::*;

/// The scalar type a `const` declaration argument must have (see
/// [`Analyzer::const_declaration_argument`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConstArgType {
    Int,
    /// `int` or `float`: Pine widens an int argument to a float parameter.
    Float,
    Bool,
    String,
}

impl ConstArgType {
    fn accepts(self, ty: &Type) -> bool {
        match self {
            ConstArgType::Int => *ty == Type::Int,
            ConstArgType::Float => matches!(ty, Type::Int | Type::Float),
            ConstArgType::Bool => *ty == Type::Bool,
            ConstArgType::String => *ty == Type::String,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            ConstArgType::Int => "int",
            ConstArgType::Float => "float",
            ConstArgType::Bool => "bool",
            ConstArgType::String => "string",
        }
    }
}

impl<'r> Analyzer<'r> {
    /// Analyze a `library("Title", overlay?)` declaration. The title must
    /// be a constant string; the optional second argument is an `overlay`
    /// boolean. Enforces the `library()`/`indicator()`/`strategy()`
    /// mutual-exclusion rule (`E0240`) and rejects a duplicate (`E0213`).
    pub(crate) fn analyze_library_decl(&mut self, decl: &ast::LibraryDecl) {
        if self.indicator.is_some() || self.strategy.is_some() {
            self.error(
                codes::E0240,
                "a script cannot declare both `library(...)` and `indicator(...)`/`strategy(...)`",
                decl.span,
                "library(), indicator(), and strategy() are mutually exclusive; keep only one",
            );
            return;
        }
        // `detect_library_kind` recorded the first library span; a second
        // `library(...)` here means a duplicate.
        if self.library.is_some_and(|s| s != decl.span) {
            self.error(
                codes::E0213,
                "duplicate `library(...)` declaration",
                decl.span,
                "a script declares its library once",
            );
            return;
        }
        // `title`, `overlay` and `dynamic_requests`, by position or by name.
        let param_names = ["title", "overlay", "dynamic_requests"];
        let (bound, overflow) = match self.bind_call_args("library", &param_names, &decl.args) {
            Some(b) => b,
            None => return,
        };
        if !overflow.is_empty() {
            let span = overflow[0].span().to(overflow.last().unwrap().span());
            self.error(
                codes::E0204,
                "`library` takes at most `title`, `overlay` and `dynamic_requests`",
                span,
                "remove the extra arguments",
            );
        }
        // Title: a `const string`. A literal, a `const` alias or any
        // const-folding string expression is accepted; a wrong type is
        // `E0205` and a non-const string `E0206`.
        match bound[0] {
            Some(ast::Expr::Str { .. }) => {}
            Some(other) => {
                let diagnostics = self.diagnostics.mark();
                match self.const_declaration_argument(
                    "library",
                    "title",
                    ConstArgType::String,
                    other,
                ) {
                    Some(InputValue::String(_)) => {}
                    _ => {
                        if !self.diagnostics.errors_since(diagnostics) {
                            self.error(
                                codes::E0205,
                                "`library` title must be a constant string",
                                other.span(),
                                "expected a const string expression",
                            );
                        }
                        return;
                    }
                }
            }
            None => {
                self.error(
                    codes::E0204,
                    "`library` requires a title argument",
                    decl.span,
                    "add a title: library(\"MyLib\")",
                );
                return;
            }
        }
        // `overlay` and `dynamic_requests` are const bools. `overlay` carries
        // no runtime effect in the headless runtime; `dynamic_requests = false`
        // forbids requests in local scopes.
        for (index, name) in [(1, "overlay"), (2, "dynamic_requests")] {
            let Some(arg) = bound[index] else {
                continue;
            };
            let expr = self.analyze_expr(arg);
            if expr.ty.is_error() {
                continue;
            }
            if !matches!(expr.ty, Type::Bool) {
                self.error(
                    codes::E0205,
                    format!(
                        "`library` {name} must be a boolean, found {}",
                        expr.ty.describe()
                    ),
                    arg.span(),
                    "expected `true` or `false`",
                );
            } else if expr.qualifier != Qualifier::Const {
                self.error(
                    codes::E0206,
                    format!(
                        "`library` {name} must be const, found {}",
                        expr.qualifier.describe()
                    ),
                    arg.span(),
                    "expected a const value",
                );
            } else if name == "dynamic_requests" {
                if let Some(InputValue::Bool(value)) = self.eval_const_input_expr(arg) {
                    self.declared_dynamic_requests = Some(value);
                }
            }
        }
        self.library = Some(decl.span);
    }

    pub(crate) fn analyze_indicator_decl(&mut self, decl: &ast::IndicatorDecl) {
        if self.library.is_some() {
            self.error(
                codes::E0240,
                "a script cannot declare both `indicator(...)` and `library(...)`",
                decl.span,
                "indicator() and library() are mutually exclusive; keep only one",
            );
            return;
        }
        if self.strategy.is_some() {
            self.error(
                codes::E0240,
                "a script cannot declare both `indicator(...)` and `strategy(...)`",
                decl.span,
                "indicator() and strategy() are mutually exclusive; keep only one",
            );
            return;
        }
        if self.indicator.is_some() {
            self.error(
                codes::E0213,
                "duplicate `indicator(...)` declaration",
                decl.span,
                "a script declares its indicator once",
            );
            return;
        }
        // Bind positional + named arguments to the full indicator signature
        // (title + the display/metadata surface). `bound[i]` aligns with
        // `param_names[i]`, so each parameter is extracted by its index. Named
        // binding means `indicator(title = "X", overlay = true)` works.
        let param_names: Vec<&'static str> = {
            let func = self
                .registry
                .lookup_function("indicator")
                .expect("indicator is a registered builtin");
            func.params.iter().map(|p| p.name).collect()
        };
        // Bind sparsely (per-parameter `Option` slots) so a named argument may
        // skip an interior optional parameter — every parameter except `title`
        // is optional, unlike a typical builtin whose optionals are strictly
        // trailing (mirrors `analyze_strategy_decl`).
        let (bound, overflow) = match self.bind_call_args("indicator", &param_names, &decl.args) {
            Some(b) => b,
            None => return,
        };
        if !overflow.is_empty() {
            let span = overflow[0].span().to(overflow.last().unwrap().span());
            self.error(
                codes::E0204,
                "additional `indicator` arguments are not supported yet",
                span,
                "only the parameter surface PineWASM implements is supported",
            );
        }
        // Index of each parameter in the signature.
        let idx = |name: &str| param_names.iter().position(|p| *p == name);
        // The title is a `const string`. Evaluate it once: a second analysis
        // would repeat its own diagnostics. An invalid title is reported and
        // the declaration is still recorded (with an empty title), so the
        // script does not also get a misleading "no declaration" `W0001`.
        let title = match bound.first().copied().flatten() {
            Some(other) => {
                let diagnostics = self.diagnostics.mark();
                match self.const_declaration_argument(
                    "indicator",
                    "title",
                    ConstArgType::String,
                    other,
                ) {
                    Some(InputValue::String(value)) => value,
                    _ => {
                        if !self.diagnostics.errors_since(diagnostics) {
                            self.error(
                                codes::E0205,
                                "`indicator` title must be a constant string",
                                other.span(),
                                "expected a const string expression",
                            );
                        }
                        String::new()
                    }
                }
            }
            None => {
                self.error(
                    codes::E0204,
                    "`indicator` requires a title argument",
                    decl.span,
                    "add a title: indicator(\"My script\")",
                );
                return;
            }
        };
        // Const-fold each provided parameter. A helper reads the const int in a
        // parameter slot within `[lo, hi]` (`E0219` on violation); an omitted
        // slot yields `None`. The purely-display params (shorttitle/overlay/…)
        // are type-checked below and otherwise carried as accepted metadata;
        // the `max_*_count` params are honored as drawing caps.
        let const_int_at = |analyzer: &mut Self, name: &str, lo: i64, hi: i64| -> Option<u32> {
            idx(name)
                .and_then(|i| bound.get(i).copied().flatten())
                .and_then(|arg| analyzer.declaration_const_int("indicator", name, arg, lo, hi))
                .map(|v| v as u32)
        };
        // Optional `max_bars_back=N`. It requests a history ceiling of `N`
        // bars (see `check_max_bars_back_offsets`); the buffer is capped at
        // `MAX_BARS_BACK_LIMIT`, so it is validated to that range.
        let max_bars_back = const_int_at(self, "max_bars_back", 1, MAX_BARS_BACK_LIMIT as i64);
        // Lines, labels, and boxes may disable retention with a zero cap.
        // Polylines have their own range of 1..=100.
        let max_lines_count = const_int_at(self, "max_lines_count", 0, DRAWING_COUNT_LIMIT);
        let max_labels_count = const_int_at(self, "max_labels_count", 0, DRAWING_COUNT_LIMIT);
        let max_boxes_count = const_int_at(self, "max_boxes_count", 0, DRAWING_COUNT_LIMIT);
        let max_polylines_count = const_int_at(self, "max_polylines_count", 1, 100);
        // The int-valued display params carry their own bounds.
        let precision = const_int_at(self, "precision", 0, 16).map(|value| value as u8);
        let _ = const_int_at(self, "calc_bars_count", 0, MAX_BARS_BACK_LIMIT as i64);
        // Compatibility field for the wire/API. The host supplies realtime updates.
        let calc_on_every_tick = false;
        // `overlay` — draw on the price pane (`true`) vs a separate pane
        // (`false`, the default). Fold the boolean literal for the host;
        // a wrong-typed value is still reported by the general type-check
        // below as `E0205`, so this only reads the value and never diagnoses.
        let overlay = idx("overlay")
            .and_then(|i| bound.get(i).copied().flatten())
            .and_then(|arg| match self.eval_const_input_expr(arg) {
                Some(InputValue::Bool(value)) => Some(value),
                _ => None,
            })
            .unwrap_or(false);
        // `scale.*` values are const-string tokens, not numeric enum values.
        // Keep raw strings invalid so misspelled tokens cannot be accepted as
        // display metadata that a host later interprets differently.
        let scale = idx("scale")
            .and_then(|i| bound.get(i).copied().flatten())
            .and_then(|arg| {
                if matches!(arg, ast::Expr::Str { .. }) {
                    self.error(
                        codes::E0205,
                        "`indicator` scale must be a scale.* constant",
                        arg.span(),
                        "use scale.left, scale.right, or scale.none",
                    );
                    return None;
                }
                let diagnostics = self.diagnostics.mark();
                match self.eval_const_input_expr(arg) {
                    Some(InputValue::String(value))
                        if ["scale.left", "scale.right", "scale.none"]
                            .contains(&value.as_str()) =>
                    {
                        Some(value)
                    }
                    Some(InputValue::String(value)) => {
                        self.e0219(
                            "indicator",
                            "scale",
                            format!("unsupported scale token {value:?}"),
                            arg.span(),
                        );
                        None
                    }
                    _ => {
                        if !self.diagnostics.errors_since(diagnostics) {
                            self.error(
                                codes::E0205,
                                "`indicator` scale must be a scale.* constant",
                                arg.span(),
                                "use scale.left, scale.right, or scale.none",
                            );
                        }
                        None
                    }
                }
            });
        if scale.as_deref() == Some("scale.none") && !overlay {
            self.e0219(
                "indicator",
                "scale",
                "scale.none is only valid when overlay is true".to_string(),
                idx("scale")
                    .and_then(|i| bound.get(i).copied().flatten())
                    .expect("scale.none came from a present argument")
                    .span(),
            );
        }
        // Type-check every remaining provided argument against its declared
        // parameter type (the string/bool metadata params) so a wrong-typed
        // literal is rejected (`E0205`) and a correct one compiles. `title`
        // (index 0) and the already-folded int params are skipped.
        let indicator_params: Vec<(String, Type)> = {
            let func = self
                .registry
                .lookup_function("indicator")
                .expect("indicator is a registered builtin");
            func.params
                .iter()
                .map(|p| (p.name.to_string(), p.ty.clone()))
                .collect()
        };
        for (i, slot) in bound.iter().enumerate() {
            let Some(arg) = slot else { continue };
            let (pname, pty) = &indicator_params[i.min(indicator_params.len() - 1)];
            // Skip parameters already folded and validated above.
            if *pty == Type::Int || pname == "title" || pname == "scale" {
                continue;
            }
            // One diagnostic per argument: a wrong type (`E0205`) first, else
            // a qualifier stronger than `const` (`E0206`), the same codes and
            // wording as the general argument checker.
            let hir = self.analyze_expr(arg);
            if hir.ty.is_error() {
                continue;
            }
            if hir.ty != *pty {
                self.error(
                    codes::E0205,
                    format!(
                        "`indicator` argument `{pname}` must be {}, found {}",
                        pty.describe(),
                        hir.ty.describe()
                    ),
                    arg.span(),
                    format!("expected {}", pty.describe()),
                );
            } else if hir.qualifier != Qualifier::Const {
                self.error(
                    codes::E0206,
                    format!(
                        "`indicator` argument `{pname}` must be const or weaker, found {}",
                        hir.qualifier.describe()
                    ),
                    arg.span(),
                    "expected a const value",
                );
            }
        }
        if let Some(limit) = max_bars_back {
            self.raise_max_bars_back(limit);
        }
        let named = |name: &str| idx(name).and_then(|i| bound.get(i).copied().flatten());
        let shorttitle =
            named("shorttitle").and_then(|arg| match self.eval_const_input_expr(arg) {
                Some(InputValue::String(value)) => Some(value),
                _ => None,
            });
        let format = named("format").and_then(|arg| match self.eval_const_input_expr(arg) {
            Some(InputValue::String(value)) => Some(value),
            _ => None,
        });
        let explicit_plot_zorder = named("explicit_plot_zorder")
            .and_then(|arg| match self.eval_const_input_expr(arg) {
                Some(InputValue::Bool(value)) => Some(value),
                _ => None,
            })
            .unwrap_or(false);
        let behind_chart = named("behind_chart")
            .and_then(|arg| match self.eval_const_input_expr(arg) {
                Some(InputValue::Bool(value)) => Some(value),
                _ => None,
            })
            .unwrap_or(true);
        self.declared_dynamic_requests =
            named("dynamic_requests").and_then(|arg| match self.eval_const_input_expr(arg) {
                Some(InputValue::Bool(value)) => Some(value),
                _ => None,
            });
        // `timeframe_gaps` is allowed only alongside a `timeframe` argument.
        if let (Some(gaps), None) = (named("timeframe_gaps"), named("timeframe")) {
            self.error(
                codes::E0204,
                "`indicator` accepts `timeframe_gaps` only with a `timeframe` argument",
                gaps.span(),
                "add a `timeframe` argument or remove `timeframe_gaps`",
            );
        }
        self.indicator = Some(IndicatorInfo {
            title,
            shorttitle,
            format,
            precision,
            scale,
            explicit_plot_zorder,
            behind_chart,
            max_bars_back,
            max_lines_count,
            max_labels_count,
            max_boxes_count,
            max_polylines_count,
            calc_on_every_tick,
            overlay,
            span: decl.span,
        });
    }

    /// Analyze a `strategy(...)` top-level declaration: bind the full
    /// parameter surface, const-fold each argument into [`StrategyInfo`], and
    /// enforce the `strategy()`/`indicator()` mutual-exclusion rule (`E0240`).
    /// A duplicate `strategy(...)` is `E0213`.
    pub(crate) fn analyze_strategy_decl(&mut self, decl: &ast::StrategyDecl) {
        if self.indicator.is_some() || self.library.is_some() {
            self.error(
                codes::E0240,
                "a script cannot declare both `strategy(...)` and `indicator(...)`/`library(...)`",
                decl.span,
                "indicator(), strategy(), and library() are mutually exclusive; keep only one",
            );
            return;
        }
        if self.strategy.is_some() {
            self.error(
                codes::E0213,
                "duplicate `strategy(...)` declaration",
                decl.span,
                "a script declares its strategy once",
            );
            return;
        }
        // Bind against the single parameter list exported by HIR. This is
        // deliberately not a second hand-maintained list: a positional
        // declaration must mean the same thing in every layer.
        let params = &STRATEGY_DECL_PARAMS;
        // Bind sparsely so a named argument may skip an interior optional
        // parameter (every parameter except `title` is optional here, unlike a
        // typical builtin whose optionals are strictly trailing).
        let (bound, overflow) = match self.bind_call_args("strategy", params, &decl.args) {
            Some(b) => b,
            None => return,
        };
        if !overflow.is_empty() {
            let span = overflow[0].span().to(overflow.last().unwrap().span());
            self.error(
                codes::E0204,
                "additional `strategy` arguments are not supported yet",
                span,
                "only the parameter surface PineWASM implements is supported",
            );
        }
        // Required title (parameter 0).
        let title = match bound.first().copied().flatten() {
            Some(other) => match self.strategy_const_string("title", other) {
                Some(value) => value,
                None => return,
            },
            None => {
                self.error(
                    codes::E0204,
                    "`strategy` requires a title argument",
                    decl.span,
                    "add a title: strategy(\"My strategy\")",
                );
                return;
            }
        };

        // Const-fold each optional parameter, holding the default when absent.
        // Name-based extraction makes the code reviewable against the
        // parameter list without fragile numeric offsets.
        let arg = |name: &str| {
            params
                .iter()
                .position(|candidate| *candidate == name)
                .and_then(|i| bound.get(i).copied().flatten())
        };
        let shorttitle = arg("shorttitle")
            .and_then(|e| self.strategy_const_string("shorttitle", e))
            .unwrap_or_else(|| title.clone());
        let overlay = arg("overlay")
            .and_then(|e| self.strategy_const_bool("overlay", e))
            .unwrap_or(false);
        let format = arg("format")
            .and_then(|e| {
                self.strategy_const_token(
                    "format",
                    e,
                    &[
                        "format.inherit",
                        "format.price",
                        "format.volume",
                        "format.percent",
                    ],
                )
            })
            .unwrap_or_else(|| "format.inherit".to_string());
        let precision = arg("precision")
            .and_then(|e| self.strategy_const_int("precision", e, 0, 16))
            .map(|v| v as u32);
        let scale = arg("scale").and_then(|e| {
            if matches!(e, ast::Expr::Str { .. }) {
                self.error(
                    codes::E0205,
                    "`strategy` scale must be a scale.* constant",
                    e.span(),
                    "use scale.left, scale.right, or scale.none",
                );
                None
            } else {
                self.strategy_const_token("scale", e, &["scale.left", "scale.right", "scale.none"])
            }
        });
        if scale.as_deref() == Some("scale.none") && !overlay {
            self.e0219(
                "strategy",
                "scale",
                "scale.none is only valid when overlay is true".to_string(),
                arg("scale")
                    .expect("scale.none came from a present argument")
                    .span(),
            );
        }
        let pyramiding = arg("pyramiding")
            .and_then(|e| self.strategy_const_int("pyramiding", e, 0, i64::from(u32::MAX)))
            .map_or(1, |v| v as u32);
        let calc_on_order_fills = arg("calc_on_order_fills")
            .and_then(|e| self.strategy_const_bool("calc_on_order_fills", e))
            .unwrap_or(false);
        let calc_on_every_tick = arg("calc_on_every_tick")
            .and_then(|e| self.strategy_const_bool("calc_on_every_tick", e))
            .unwrap_or(false);
        let max_bars_back = arg("max_bars_back")
            .and_then(|e| {
                self.strategy_const_int("max_bars_back", e, 0, MAX_BARS_BACK_LIMIT as i64)
            })
            .and_then(|v| (v > 0).then_some(v as u32));
        let backtest_fill_limits_assumption = arg("backtest_fill_limits_assumption")
            .and_then(|e| {
                self.strategy_const_int(
                    "backtest_fill_limits_assumption",
                    e,
                    0,
                    i64::from(u32::MAX),
                )
            })
            .map_or(0, |v| v as u32);
        let default_qty_type = arg("default_qty_type")
            .and_then(|e| {
                self.strategy_const_enum(
                    "default_qty_type",
                    e,
                    &["fixed", "cash", "percent_of_equity"],
                )
            })
            .unwrap_or_else(|| "fixed".to_string());
        let default_qty_value = arg("default_qty_value")
            .and_then(|e| self.strategy_const_float("default_qty_value", e, true))
            .unwrap_or(1.0);
        let initial_capital = arg("initial_capital")
            .and_then(|e| self.strategy_const_float("initial_capital", e, true))
            .unwrap_or(1_000_000.0);
        let currency = arg("currency")
            .and_then(|e| self.strategy_const_currency(e))
            .unwrap_or_else(|| "NONE".to_string());
        let slippage = arg("slippage")
            .and_then(|e| self.strategy_const_int("slippage", e, 0, i64::from(u32::MAX)))
            .map_or(0, |v| v as u32);
        let commission_type = arg("commission_type")
            .and_then(|e| {
                self.strategy_const_enum(
                    "commission_type",
                    e,
                    &["percent", "cash_per_contract", "cash_per_order"],
                )
            })
            .unwrap_or_else(|| "percent".to_string());
        let commission_value_arg = arg("commission_value");
        let commission_value = commission_value_arg
            .and_then(|e| self.strategy_const_float("commission_value", e, false))
            .filter(|value| {
                if *value < 0.0 {
                    self.e0219(
                        "strategy",
                        "commission_value",
                        format!("must be at least 0, found {value}"),
                        commission_value_arg
                            .expect("negative commission came from a present argument")
                            .span(),
                    );
                    false
                } else {
                    true
                }
            })
            .unwrap_or(0.0);
        let process_orders_on_close = arg("process_orders_on_close")
            .and_then(|e| self.strategy_const_bool("process_orders_on_close", e))
            .unwrap_or(false);
        let close_entries_rule = arg("close_entries_rule")
            .and_then(|e| self.strategy_const_token("close_entries_rule", e, &["FIFO", "ANY"]))
            .unwrap_or_else(|| "FIFO".to_string());
        let margin_long = arg("margin_long")
            .and_then(|e| self.strategy_const_float("margin_long", e, false))
            .filter(|value| {
                if *value < 0.0 {
                    self.e0219(
                        "strategy",
                        "margin_long",
                        format!("must be at least 0, found {value}"),
                        arg("margin_long").expect("present margin_long").span(),
                    );
                    false
                } else {
                    true
                }
            })
            .unwrap_or(100.0);
        let margin_short = arg("margin_short")
            .and_then(|e| self.strategy_const_float("margin_short", e, false))
            .filter(|value| {
                if *value < 0.0 {
                    self.e0219(
                        "strategy",
                        "margin_short",
                        format!("must be at least 0, found {value}"),
                        arg("margin_short").expect("present margin_short").span(),
                    );
                    false
                } else {
                    true
                }
            })
            .unwrap_or(100.0);
        let explicit_plot_zorder = arg("explicit_plot_zorder")
            .and_then(|e| self.strategy_const_bool("explicit_plot_zorder", e))
            .unwrap_or(false);
        let count = |analyzer: &mut Self, name: &str| {
            arg(name)
                .and_then(|e| analyzer.strategy_const_int(name, e, 0, DRAWING_COUNT_LIMIT))
                .map(|v| v as u32)
                .or(Some(50))
        };
        let max_lines_count = count(self, "max_lines_count");
        let max_labels_count = count(self, "max_labels_count");
        let max_boxes_count = count(self, "max_boxes_count");
        let max_polylines_count = arg("max_polylines_count")
            .and_then(|e| self.strategy_const_int("max_polylines_count", e, 1, 100))
            .map(|v| v as u32)
            .or(Some(50));
        let calc_bars_count = arg("calc_bars_count")
            .and_then(|e| self.strategy_const_int("calc_bars_count", e, 0, i64::from(u32::MAX)))
            .map_or(0, |v| v as u32);
        let risk_free_rate = arg("risk_free_rate")
            .and_then(|e| self.strategy_const_float("risk_free_rate", e, false))
            .unwrap_or(2.0);
        let use_bar_magnifier = arg("use_bar_magnifier")
            .and_then(|e| self.strategy_const_bool("use_bar_magnifier", e))
            .unwrap_or(false);
        let fill_orders_on_standard_ohlc = arg("fill_orders_on_standard_ohlc")
            .and_then(|e| self.strategy_const_bool("fill_orders_on_standard_ohlc", e))
            .unwrap_or(false);
        let dynamic_requests = arg("dynamic_requests")
            .and_then(|e| self.strategy_const_bool("dynamic_requests", e))
            .unwrap_or(true);
        let behind_chart = arg("behind_chart")
            .and_then(|e| self.strategy_const_bool("behind_chart", e))
            .unwrap_or(true);
        // `calc_on_every_history_tick` is a `strategy()` parameter. A host
        // `StrategyRunOverrides` value still wins; the declaration seeds the
        // default.
        let calc_on_every_history_tick = arg("calc_on_every_history_tick")
            .and_then(|e| self.strategy_const_bool("calc_on_every_history_tick", e))
            .unwrap_or(false);
        if let Some(limit) = max_bars_back {
            self.raise_max_bars_back(limit);
        }

        self.strategy = Some(StrategyInfo {
            title,
            shorttitle,
            overlay,
            format,
            precision,
            scale,
            max_bars_back,
            initial_capital,
            default_qty_type,
            default_qty_value,
            commission_type,
            commission_value,
            slippage,
            pyramiding,
            backtest_fill_limits_assumption,
            process_orders_on_close,
            close_entries_rule,
            calc_on_every_tick,
            calc_on_order_fills,
            currency,
            margin_long,
            margin_short,
            explicit_plot_zorder,
            max_lines_count,
            max_labels_count,
            max_boxes_count,
            max_polylines_count,
            calc_bars_count,
            risk_free_rate,
            use_bar_magnifier,
            fill_orders_on_standard_ohlc,
            dynamic_requests,
            behind_chart,
            calc_on_every_history_tick,
            // Filled in by `analyze_program` from the Program-level annotation.
            alert_message: String::new(),
            span: decl.span,
        });
    }

    /// Fold a declaration argument whose signature requires a `const`
    /// value (`indicator`/`strategy`/`library` metadata), or report why it is
    /// not one. Shared so every declaration reports the two invalid classes
    /// the same way the general argument checker does:
    ///
    /// - a value of the wrong type is `E0205`
    ///   (`` `f` argument `p` must be int, found string ``);
    /// - a value of the right type with an `input`, `simple` or `series`
    ///   qualifier is `E0206`
    ///   (`` `f` argument `p` must be const or weaker, found series ``).
    ///
    /// Returns `None` without a diagnostic when the expression is a well-typed
    /// const that still does not fold, or when analyzing it already reported
    /// an error (no cascade); the caller then reports its own reason.
    /// An int setting takes the whole part of a statically-int fold
    /// (`7 / 2`), as [`Self::eval_const_int_setting`] does.
    pub(crate) fn const_declaration_argument(
        &mut self,
        func: &str,
        param: &str,
        expected: ConstArgType,
        arg: &ast::Expr,
    ) -> Option<InputValue> {
        let diagnostics = self.diagnostics.mark();
        let (value, ty, qualifier) = self.eval_const_input_expr_checked(arg);
        let analysis_failed = ty.is_error() || self.diagnostics.errors_since(diagnostics);
        if analysis_failed {
            return None;
        }
        if !expected.accepts(&ty) {
            self.error(
                codes::E0205,
                format!(
                    "`{func}` argument `{param}` must be {}, found {}",
                    expected.describe(),
                    ty.describe()
                ),
                arg.span(),
                format!("expected {}", expected.describe()),
            );
            return None;
        }
        if qualifier != Qualifier::Const {
            self.error(
                codes::E0206,
                format!(
                    "`{func}` argument `{param}` must be const or weaker, found {}",
                    qualifier.describe()
                ),
                arg.span(),
                "expected a const value",
            );
            return None;
        }
        match (expected, value) {
            (ConstArgType::Int, Some(InputValue::Float(value))) => {
                let whole = value.trunc();
                (whole >= i64::MIN as f64 && whole < i64::MAX as f64)
                    .then_some(InputValue::Int(whole as i64))
            }
            (_, value) => value,
        }
    }

    /// A const-string argument to `strategy(...)`; `E0205` otherwise. Accepts a
    /// string literal or any expression that const-folds to a `ConstString` —
    /// so a named constant such as `currency.USD` (which folds to its ISO code
    /// `"USD"`) or `strategy.fixed` is usable wherever a raw string was.
    pub(crate) fn strategy_const_string(&mut self, param: &str, arg: &ast::Expr) -> Option<String> {
        let diagnostics = self.diagnostics.mark();
        if let Some(InputValue::String(value)) =
            self.const_declaration_argument("strategy", param, ConstArgType::String, arg)
        {
            return Some(value);
        }
        if !self.diagnostics.errors_since(diagnostics) {
            self.error(
                codes::E0205,
                format!("`strategy` {param} must be a constant string"),
                arg.span(),
                "expected a const string expression",
            );
        }
        None
    }

    /// A const int strategy declaration argument within an inclusive range.
    pub(crate) fn strategy_const_int(
        &mut self,
        param: &str,
        arg: &ast::Expr,
        min: i64,
        max: i64,
    ) -> Option<i64> {
        self.declaration_const_int("strategy", param, arg, min, max)
    }

    /// A const int declaration argument (`indicator`/`strategy`) within an
    /// inclusive range. Unlike the stateful-TA literal parameters, declaration
    /// metadata accepts any genuinely const expression or const alias, as the
    /// parameter list specifies. A wrong type is `E0205`, a non-const value
    /// `E0206`, and an out-of-range value `E0219`.
    pub(crate) fn declaration_const_int(
        &mut self,
        func: &str,
        param: &str,
        arg: &ast::Expr,
        min: i64,
        max: i64,
    ) -> Option<i64> {
        let diagnostics = self.diagnostics.mark();
        let value = match self.const_declaration_argument(func, param, ConstArgType::Int, arg) {
            Some(InputValue::Int(value)) => value,
            _ => {
                if !self.diagnostics.errors_since(diagnostics) {
                    self.e0219(
                        func,
                        param,
                        "must be a const int expression".to_string(),
                        arg.span(),
                    );
                }
                return None;
            }
        };
        if value < min {
            self.e0219(
                func,
                param,
                format!("must be at least {min}, found {value}"),
                arg.span(),
            );
            return None;
        }
        if value > max {
            self.e0219(
                func,
                param,
                format!("must be at most {max}, found {value}"),
                arg.span(),
            );
            return None;
        }
        Some(value)
    }

    pub(crate) fn strategy_const_float(
        &mut self,
        param: &str,
        arg: &ast::Expr,
        positive: bool,
    ) -> Option<f64> {
        let diagnostics = self.diagnostics.mark();
        let value =
            match self.const_declaration_argument("strategy", param, ConstArgType::Float, arg) {
                Some(InputValue::Int(value)) => value as f64,
                Some(InputValue::Float(value)) => value,
                _ => {
                    if !self.diagnostics.errors_since(diagnostics) {
                        self.e0219(
                            "strategy",
                            param,
                            "must be a const numeric expression".to_string(),
                            arg.span(),
                        );
                    }
                    return None;
                }
            };
        if !value.is_finite() {
            self.e0219(
                "strategy",
                param,
                "must be a finite number".to_string(),
                arg.span(),
            );
            return None;
        }
        if positive && value <= 0.0 {
            self.e0219(
                "strategy",
                param,
                format!("must be greater than zero, found {value}"),
                arg.span(),
            );
            return None;
        }
        Some(value)
    }

    pub(crate) fn strategy_const_bool(&mut self, param: &str, arg: &ast::Expr) -> Option<bool> {
        let diagnostics = self.diagnostics.mark();
        match self.const_declaration_argument("strategy", param, ConstArgType::Bool, arg) {
            Some(InputValue::Bool(value)) => Some(value),
            _ => {
                if !self.diagnostics.errors_since(diagnostics) {
                    self.e0219(
                        "strategy",
                        param,
                        "must be a const bool expression".to_string(),
                        arg.span(),
                    );
                }
                None
            }
        }
    }

    /// A const string argument restricted to a fixed set of tokens (the
    /// `strategy.*` enum-like string parameters). `E0205` for a non-string,
    /// `E0219` for an out-of-set token. The `allowed` set holds the bare tails
    /// (`"fixed"`, `"cash_per_order"`, …); a namespaced named constant
    /// (`strategy.fixed`, `strategy.commission.cash_per_order`) folds to the
    /// full dotted token, so it is normalized to its trailing segment before
    /// the membership check — both the raw string and the named constant then
    /// resolve to the same value.
    pub(crate) fn strategy_const_enum(
        &mut self,
        param: &str,
        arg: &ast::Expr,
        allowed: &[&str],
    ) -> Option<String> {
        let raw = self.strategy_const_string(param, arg)?;
        // A `strategy.<tail>` / `strategy.commission.<tail>` token normalizes to
        // its final segment; a bare string passes through unchanged.
        let value = raw
            .strip_prefix("strategy.commission.")
            .or_else(|| raw.strip_prefix("strategy."))
            .unwrap_or(&raw)
            .to_string();
        if allowed.contains(&value.as_str()) {
            Some(value)
        } else {
            self.e0219(
                "strategy",
                param,
                format!("must be one of {allowed:?}, found {value:?}"),
                arg.span(),
            );
            None
        }
    }

    /// A const string restricted to exact token spellings. Unlike
    /// `strategy_const_enum`, this intentionally retains namespaces and case
    /// (`format.price`, `scale.right`, `FIFO`, `ANY`).
    pub(crate) fn strategy_const_token(
        &mut self,
        param: &str,
        arg: &ast::Expr,
        allowed: &[&str],
    ) -> Option<String> {
        let value = self.strategy_const_string(param, arg)?;
        if allowed.contains(&value.as_str()) {
            Some(value)
        } else {
            self.e0219(
                "strategy",
                param,
                format!("must be one of {allowed:?}, found {value:?}"),
                arg.span(),
            );
            None
        }
    }

    /// Validate an account currency code against the supported code list;
    /// any other text is rejected.
    pub(crate) fn strategy_const_currency(&mut self, arg: &ast::Expr) -> Option<String> {
        const CODES: &[&str] = &[
            "AED", "ARS", "AUD", "BDT", "BHD", "BRL", "BTC", "CAD", "CHF", "CLP", "CNY", "COP",
            "CZK", "DKK", "EGP", "ETH", "EUR", "GBP", "HKD", "HUF", "IDR", "ILS", "INR", "ISK",
            "JPY", "KES", "KRW", "KWD", "LKR", "MAD", "MXN", "MYR", "NGN", "NOK", "NONE", "NZD",
            "PEN", "PHP", "PKR", "PLN", "QAR", "RON", "RSD", "RUB", "SAR", "SEK", "SGD", "THB",
            "TND", "TRY", "TWD", "USD", "USDT", "VES", "VND", "ZAR",
        ];
        let value = self.strategy_const_string("currency", arg)?;
        if CODES.contains(&value.as_str()) {
            Some(value)
        } else {
            self.e0219(
                "strategy",
                "currency",
                format!("unsupported account currency {value:?}"),
                arg.span(),
            );
            None
        }
    }

    /// Collect the top-level function definitions into [`Self::functions`],
    /// validating each: redefining a built-in is `E0211`, a second definition
    /// with the same name and the same signature (parameter count and
    /// declared types) is `E0224`, and a duplicate parameter name within one
    /// definition is `E0224`. Definitions whose signatures differ are
    /// overloads of one name. `plot`/`indicator` inside a body is checked at
    /// expansion time (an inlined body runs in statement context, so the
    /// effect-call rules apply there).
    pub(crate) fn collect_functions(&mut self, program: &ast::Program) {
        for item in &program.items {
            let ast::Item::FuncDef(def) = item else {
                continue;
            };
            self.diagnostics
                .extend(validate_function_parameter_declarations(def));
            // A `method` definition is collected separately (keyed by receiver
            // type + name) so `obj.f(args)` resolves; it does not occupy the
            // bare-name function namespace.
            if def.receiver.is_some() {
                self.collect_method(def);
                continue;
            }
            if self.registry.lookup_function(&def.name.name).is_some()
                || self.registry.lookup_series(&def.name.name).is_some()
                || self.registry.lookup_constant(&def.name.name).is_some()
                || is_cast_name(&def.name.name)
            {
                self.error(
                    codes::E0211,
                    format!("cannot redefine built-in `{}`", def.name.name),
                    def.name.span,
                    "choose a distinct user-function name",
                );
            }
            // Duplicate parameter names within this definition.
            let mut seen_params: HashMap<&str, Span> = HashMap::new();
            let mut params_ok = true;
            for param in &def.params {
                // Parameters named `_` are unused and may repeat, like variables.
                if let Some(first) = seen_params
                    .get(param.name.name.as_str())
                    .filter(|_| param.name.name != DISCARD_NAME)
                {
                    self.diagnostics.push(
                        Diagnostic::error(
                            codes::E0224,
                            format!("duplicate parameter `{}`", param.name.name),
                        )
                        .with_label(param.name.span, "declared again here")
                        .with_label(*first, "first declared here"),
                    );
                    params_ok = false;
                } else {
                    seen_params.insert(param.name.name.as_str(), param.name.span);
                }
            }

            // A second definition with an identical signature. Different
            // parameter counts or declared types make an overload instead.
            if let Some(existing) = self
                .functions
                .get(&def.name.name)
                .and_then(|defs| defs.iter().find(|d| same_signature(d, def)))
            {
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::E0224,
                        format!("function `{}` is already defined", def.name.name),
                    )
                    .with_label(def.name.span, "redefined here")
                    .with_label(existing.name.span, "first defined here"),
                );
                continue;
            }
            if !params_ok {
                // A definition with bad params is still registered (under its
                // name) so call sites report arity, not "unknown function";
                // but its body will not be inlined cleanly. Keeping the entry
                // avoids a misleading cascade.
            }
            self.functions
                .entry(def.name.name.clone())
                .or_default()
                .push(def.clone());
        }
    }

    /// Collect one `method` definition. Validates duplicate parameter
    /// names (`E0224`) and resolves the receiver type; an unknown receiver type
    /// (a name that is neither a UDT nor a collection form) is `E0237`. The
    /// method registers under `(receiver key, method name)`; a duplicate
    /// method for the same receiver type is `E0224`.
    pub(crate) fn collect_method(&mut self, def: &ast::FuncDef) {
        // Duplicate parameter names within this method (receiver included).
        let mut seen_params: HashMap<&str, Span> = HashMap::new();
        for param in &def.params {
            // Parameters named `_` are unused and may repeat, like variables.
            if let Some(first) = seen_params
                .get(param.name.name.as_str())
                .filter(|_| param.name.name != DISCARD_NAME)
            {
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::E0224,
                        format!("duplicate parameter `{}`", param.name.name),
                    )
                    .with_label(param.name.span, "declared again here")
                    .with_label(*first, "first declared here"),
                );
            } else {
                seen_params.insert(param.name.name.as_str(), param.name.span);
            }
        }

        let receiver = def
            .receiver
            .as_ref()
            .expect("collect_method is only called for methods");
        let Some(key) = self.receiver_type_key(receiver) else {
            // The receiver names a type that does not exist.
            let name = match &receiver.kind {
                ast::FieldTypeKind::Named(n) => n.clone(),
                _ => fmt_field_type_key(receiver),
            };
            let mut diag = Diagnostic::error(
                codes::E0237,
                format!(
                    "unknown receiver type `{name}` on method `{}`",
                    def.name.name
                ),
            )
            .with_label(
                receiver.span,
                "not a user-defined type, enum or collection type",
            );
            let types = &self.types;
            if let Some(suggestion) = self
                .suggestions
                .among(&name, || types.keys().map(|known| Some(known.as_str())))
            {
                diag = diag.with_note(format!("did you mean `{suggestion}`?"));
            }
            self.diagnostics.push(diag);
            return;
        };

        // A method on the same receiver type may be overloaded by its other
        // parameters; only one with the same signature is a redefinition.
        let map_key = (key.clone(), def.name.name.clone());
        let overloads = self.methods.entry(map_key).or_default();
        if let Some(existing) = overloads.iter().find(|d| same_signature(d, def)) {
            let first = existing.name.span;
            self.diagnostics.push(
                Diagnostic::error(
                    codes::E0224,
                    format!("method `{}` is already defined for `{key}`", def.name.name),
                )
                .with_label(def.name.span, "redefined here")
                .with_label(first, "first defined here"),
            );
            return;
        }
        overloads.push(def.clone());
    }

    /// The canonical receiver-type key for a method's declared receiver, or
    /// `None` when the receiver names a type that does not exist. Scalar,
    /// `color`, and collection receivers are always valid keys (a method may be
    /// declared on `array<float>`); a bare identifier must name a collected UDT
    /// or enum.
    pub(crate) fn receiver_type_key(&self, ty: &ast::FieldType) -> Option<String> {
        match &ty.kind {
            ast::FieldTypeKind::Named(name) => self
                .object_type_name(name)
                .or_else(|| self.enum_type_name(name)),
            _ => {
                let mut ty = ty.clone();
                self.qualify_library_type(&mut ty);
                Some(fmt_field_type_key(&ty))
            }
        }
    }

    /// The canonical receiver-type key for a receiver **value's** resolved
    /// [`Type`]. Must agree with [`Self::receiver_type_key`] so a method
    /// declared on `array<float>` (or a UDT) is found from a value of that type.
    pub(crate) fn value_type_key(ty: &Type) -> String {
        match ty {
            Type::Object(name) => name.clone(),
            other => other.describe(),
        }
    }

    /// Collect every top-level `EnumDef`, validating each. A duplicate enum
    /// name is `E0239`; a duplicate member within one enum is `E0239`. A
    /// well-formed enum registers its members (position = variant index) so
    /// `Name.member` references and enum-typed `na` annotations resolve. A
    /// duplicate member is dropped from the registered list so its index does
    /// not shadow the first declaration.
    pub(crate) fn collect_enums(&mut self, program: &ast::Program) {
        for item in &program.items {
            let ast::Item::EnumDef(decl) = item else {
                continue;
            };
            let enum_name = self.lib_ctx.last().map_or_else(
                || decl.name.name.clone(),
                |frame| format!("@lib{}.{}", frame.unit, decl.name.name),
            );
            // Duplicate enum name.
            if let Some(existing) = self.enums.get(&enum_name) {
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::E0239,
                        format!("enum `{}` is already defined", decl.name.name),
                    )
                    .with_label(decl.name.span, "redefined here")
                    .with_label(existing.name_span, "first defined here"),
                );
                continue;
            }
            // Unlike a UDT, an enum may not take the name of a built-in type
            // or namespace. The enum is still registered, so its uses resolve
            // and report nothing further.
            if self.registry.is_builtin_type_or_namespace(&decl.name.name) {
                self.error(
                    codes::E0239,
                    format!(
                        "enum `{}` has the name of a built-in type or namespace",
                        decl.name.name
                    ),
                    decl.name.span,
                    "an enum needs a name no built-in type or namespace uses",
                );
            }

            // Members: reject duplicates within this enum; keep the first.
            let mut variants: Vec<EnumVariantInfo> = Vec::new();
            let mut seen: HashMap<&str, Span> = HashMap::new();
            for variant in &decl.variants {
                if let Some(first) = seen.get(variant.name.name.as_str()) {
                    self.diagnostics.push(
                        Diagnostic::error(
                            codes::E0239,
                            format!(
                                "duplicate member `{}` in enum `{}`",
                                variant.name.name, decl.name.name
                            ),
                        )
                        .with_label(variant.name.span, "declared again here")
                        .with_label(*first, "first declared here"),
                    );
                } else {
                    seen.insert(variant.name.name.as_str(), variant.name.span);
                    variants.push(EnumVariantInfo {
                        name: variant.name.name.clone(),
                        title: variant.title.clone(),
                    });
                }
            }
            self.enums.insert(
                enum_name,
                EnumInfo {
                    variants,
                    name_span: decl.name.span,
                },
            );
        }
    }

    /// Collect every top-level `TypeDef`, validating each. A duplicate
    /// type name — or a name already taken by an enum — is `E0236`; a duplicate
    /// field within one type is `E0236`; an unknown field type is `E0236`. Each
    /// field's declared type is resolved to a [`Type`] and its optional default
    /// is analyzed (in the global scope) and coerced to that type (`E0236` on a
    /// type mismatch). A well-formed type registers its fields (position = slot
    /// index) so `Name.new(...)` and `obj.field` accesses resolve.
    pub(crate) fn collect_types(&mut self, program: &ast::Program) {
        for item in &program.items {
            let ast::Item::TypeDef(decl) = item else {
                continue;
            };
            let type_name = self.lib_ctx.last().map_or_else(
                || decl.name.name.clone(),
                |frame| format!("@lib{}.{}", frame.unit, decl.name.name),
            );
            // Duplicate type name (or a name already taken by an enum).
            if let Some(existing) = self.types.get(&type_name) {
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::E0236,
                        format!("type `{}` is already defined", decl.name.name),
                    )
                    .with_label(decl.name.span, "redefined here")
                    .with_label(existing.name_span, "first defined here"),
                );
                continue;
            }
            if let Some(existing) = self.enums.get(&decl.name.name) {
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::E0236,
                        format!("`{}` is already defined as an enum", decl.name.name),
                    )
                    .with_label(decl.name.span, "type declared here")
                    .with_label(existing.name_span, "enum first defined here"),
                );
                continue;
            }

            let mut fields: Vec<FieldInfo> = Vec::new();
            let mut seen: HashMap<&str, Span> = HashMap::new();
            for field in &decl.fields {
                if let Some(first) = seen.get(field.name.name.as_str()) {
                    self.diagnostics.push(
                        Diagnostic::error(
                            codes::E0236,
                            format!(
                                "duplicate field `{}` in type `{}`",
                                field.name.name, decl.name.name
                            ),
                        )
                        .with_label(field.name.span, "declared again here")
                        .with_label(*first, "first declared here"),
                    );
                    continue;
                }
                seen.insert(field.name.name.as_str(), field.name.span);

                let Some(field_ty) = self.resolve_field_type(&field.ty, &decl.name.name) else {
                    continue;
                };
                // Analyze and coerce the optional default. Defaults are const
                // expressions evaluated at construction; analyze in the global
                // scope. A UDT/collection-typed default may only be `na` (there
                // is no literal for those at declaration time).
                let default = field
                    .default
                    .as_ref()
                    .and_then(|expr| self.analyze_field_default(expr, &field_ty, field.name.span));
                fields.push(FieldInfo {
                    name: field.name.name.clone(),
                    ty: field_ty,
                    default,
                    span: field.span,
                });
            }

            let type_id = self.object_types.len() as u32;
            self.object_types.push(ObjectTypeDecl {
                name: type_name.clone(),
                field_count: fields.len() as u32,
                field_lanes: Vec::new(),
            });
            self.types.insert(
                type_name,
                TypeInfo {
                    type_id,
                    fields,
                    name_span: decl.name.span,
                },
            );
        }
    }

    /// Resolve a surface [`ast::FieldType`] to a [`Type`], reporting `E0236` for
    /// an unknown named type. `owner` is the enclosing type name (for the
    /// diagnostic). A named type may be another UDT (including the owning type
    /// itself, for self-referential fields such as a linked-list `next`) or an
    /// enum.
    pub(crate) fn resolve_field_type(&mut self, ty: &ast::FieldType, owner: &str) -> Option<Type> {
        match &ty.kind {
            ast::FieldTypeKind::Scalar(a) => Some(match a {
                ast::AnnotatedType::Int => Type::Int,
                ast::AnnotatedType::Float => Type::Float,
                ast::AnnotatedType::Bool => Type::Bool,
                ast::AnnotatedType::String => Type::String,
            }),
            ast::FieldTypeKind::Color => Some(Type::Color),
            ast::FieldTypeKind::Array(elem) => {
                let elem = self.resolve_field_type(elem, owner)?;
                Some(Type::Array(Box::new(elem)))
            }
            ast::FieldTypeKind::Matrix(elem) => {
                let elem = self.resolve_field_type(elem, owner)?;
                Some(Type::Matrix(Box::new(elem)))
            }
            ast::FieldTypeKind::Map(key_ty, value) => {
                let key = self.resolve_field_type(key_ty, owner)?;
                let key = self.check_map_key_type(key, key_ty.span)?;
                let value = self.resolve_field_type(value, owner)?;
                Some(Type::Map(Box::new(key), Box::new(value)))
            }
            ast::FieldTypeKind::Named(name) => {
                // The owning type is in scope for self-reference even though it
                // is not yet inserted into `self.types` (it is being collected).
                if let Some(resolved) = self.object_type_name(name) {
                    Some(Type::Object(resolved))
                } else if name == owner {
                    Some(Type::Object(name.clone()))
                } else if let Some(resolved) = self.enum_type_name(name) {
                    Some(Type::Enum(resolved))
                } else {
                    self.error(
                        codes::E0236,
                        format!("unknown field type `{name}`"),
                        ty.span,
                        "expected a scalar type, `array<T>`, or a declared type/enum name",
                    );
                    None
                }
            }
        }
    }

    /// Accept `key` as a map key type only when it is a scalar value type
    /// (`int`, `float`, `bool`, `string`, `color`) or an enum, as
    /// `map.new<K, V>` requires; anything else (a UDT, a collection) is
    /// `E0234`.
    fn check_map_key_type(&mut self, key: Type, span: Span) -> Option<Type> {
        if registry::scalar_type_tag(&key).is_some() {
            return Some(key);
        }
        self.error(
            codes::E0234,
            format!(
                "map key type `{}` must be a scalar or enum type",
                key.describe()
            ),
            span,
            "map keys are int, float, bool, string, color, or an enum",
        );
        None
    }

    /// Analyze a field's default-value expression in the global scope and
    /// coerce it to the declared field type. An `int` default widens to a
    /// `float` field (`IntToFloat`); any other mismatch is `E0236`. Returns the
    /// coerced default, or `None` on a mismatch (the field then has no default).
    pub(crate) fn analyze_field_default(
        &mut self,
        expr: &ast::Expr,
        field_ty: &Type,
        field_span: Span,
    ) -> Option<HirExpr> {
        if !self.is_field_default_shape(expr) {
            self.error(
                codes::E0236,
                "a field default must be a literal, a built-in variable or constant, or an enum member",
                expr.span(),
                "a function call or other expression is not allowed as a field default",
            );
            return None;
        }
        let value = self.analyze_expr_expecting(expr, Some(field_ty));
        if value.ty.is_error() {
            return None;
        }
        if value.ty == *field_ty {
            return Some(value);
        }
        if *field_ty == Type::Float && value.ty == Type::Int {
            return Some(Self::coerce_int_to_float(value));
        }
        self.diagnostics.push(
            Diagnostic::error(
                codes::E0236,
                format!(
                    "field default type {} does not match declared type {}",
                    value.ty.describe(),
                    field_ty.describe()
                ),
            )
            .with_label(
                field_span,
                format!("field declared as {}", field_ty.describe()),
            )
            .with_label(expr.span(), format!("default is {}", value.ty.describe())),
        );
        None
    }

    /// Whether `expr` has a shape allowed as a UDT field default: a literal
    /// (with a sign before a numeric one), `na`, a built-in variable or
    /// constant, or an enum member. A function call or any other expression
    /// is not allowed.
    fn is_field_default_shape(&self, expr: &ast::Expr) -> bool {
        match expr {
            ast::Expr::Int { .. }
            | ast::Expr::Float { .. }
            | ast::Expr::Bool { .. }
            | ast::Expr::Na { .. }
            | ast::Expr::Str { .. }
            | ast::Expr::Color { .. } => true,
            ast::Expr::Unary {
                op: ast::UnaryOp::Neg | ast::UnaryOp::Pos,
                operand,
                ..
            } => matches!(**operand, ast::Expr::Int { .. } | ast::Expr::Float { .. }),
            ast::Expr::Group { inner, .. } => self.is_field_default_shape(inner),
            ast::Expr::Ident(ident) => {
                self.registry.lookup_series(&ident.name).is_some()
                    || self.registry.lookup_constant(&ident.name).is_some()
                    || ident.name.rsplit_once('.').is_some_and(|(name, member)| {
                        self.enum_type_name(name)
                            .is_some_and(|name| self.enums[&name].variant_index(member).is_some())
                    })
            }
            _ => false,
        }
    }

    // -- statements ---------------------------------------------------------

    /// Analyze one statement and append its HIR (preceded by any control-flow
    /// statements desugared from `if`/`switch` **expressions** in its own
    /// expressions) to `out`. `pending_stmts` is empty at entry and exit: any
    /// buffer left by an outer statement is saved and restored so nested
    /// analysis cannot leak pending statements across boundaries.
    pub(crate) fn analyze_stmt_into(&mut self, stmt: &ast::Stmt, out: &mut Vec<HirStmt>) {
        let saved = std::mem::take(&mut self.pending_stmts);
        // This statement's pending statements run just before it, so call
        // arguments may be captured in order again (see `arg_order.rs`).
        let produced = self.with_arg_order_captures(|this| this.analyze_stmt(stmt));
        // Record origins before any later statement can flatten them.
        self.observe_temporal_pending(&self.pending_stmts);
        if let Some(statement) = &produced {
            self.observe_temporal_pending(std::slice::from_ref(statement));
        }
        // Drain the desugared control flow first, then the statement itself.
        out.append(&mut self.pending_stmts);
        if let Some(s) = produced {
            out.push(s);
        }
        self.pending_stmts = saved;
    }

    pub(crate) fn analyze_stmt(&mut self, stmt: &ast::Stmt) -> Option<HirStmt> {
        let is_block_tail = std::mem::take(&mut self.block_tail);
        // A compound statement recurses into its blocks; a simple one only
        // into its expressions, which are charged where they are analyzed.
        if std::mem::take(&mut self.else_if_link) {
            // An `else if` link recurses like any nested statement. It is
            // charged one unit of the shared budget, so the links and the
            // expressions they hold add up on the same stack-depth measure.
            if !self.enter_nesting(1, stmt.span()) {
                return None;
            }
            let analyzed = self.analyze_stmt_dispatch(stmt, is_block_tail);
            self.leave_nesting(1);
            return analyzed;
        }
        // A compound statement recurses into its blocks; a simple one only
        // into its expressions, which are charged where they are analyzed.
        let cost = match stmt {
            ast::Stmt::If(_)
            | ast::Stmt::Once(_)
            | ast::Stmt::For(_)
            | ast::Stmt::ForIn(_)
            | ast::Stmt::While(_)
            | ast::Stmt::Switch(_) => 2,
            _ => 0,
        };
        if !self.enter_nesting(cost, stmt.span()) {
            return None;
        }
        let analyzed = self.analyze_stmt_dispatch(stmt, is_block_tail);
        self.leave_nesting(cost);
        analyzed
    }

    fn analyze_stmt_dispatch(&mut self, stmt: &ast::Stmt, is_block_tail: bool) -> Option<HirStmt> {
        match stmt {
            ast::Stmt::Assign {
                name,
                value,
                is_var,
                is_varip,
                annotation,
                span,
            } => self.analyze_assign(name, value, *is_var, *is_varip, annotation.as_ref(), *span),
            ast::Stmt::Reassign { name, value, span } => self.analyze_reassign(name, value, *span),
            ast::Stmt::Destructure {
                targets,
                value,
                span,
            } => self.analyze_destructure(targets, value, *span),
            ast::Stmt::Expr { expr, span } => self.analyze_expr_stmt(expr, *span, is_block_tail),
            ast::Stmt::If(if_stmt) => self.analyze_if(if_stmt),
            ast::Stmt::Once(once) => self.analyze_once(once),
            ast::Stmt::For(for_stmt) => self.analyze_for(for_stmt),
            ast::Stmt::ForIn(for_in) => self.analyze_for_in(for_in),
            ast::Stmt::While(while_stmt) => self.analyze_while(while_stmt),
            ast::Stmt::Switch(switch) => self.analyze_switch_stmt(switch),
            ast::Stmt::Break { span } => self.analyze_break_continue(true, *span),
            ast::Stmt::Continue { span } => self.analyze_break_continue(false, *span),
        }
    }

    /// Resolve a variable-declaration [`ast::TypeAnnotation`] to its [`Type`].
    /// A scalar keyword or `color` maps directly; an array annotation
    /// (`array<T>` / `T[]`) resolves its element like a field type; a named
    /// annotation names a seeded drawing object type (`line`/`label`/`box`/
    /// `table`/`linefill`/`polyline`/`chart.point`), a user-defined type, or
    /// an enum. An unknown name is `E0236` and resolves to `Type::Error` so
    /// the initializer still analyzes.
    pub(crate) fn resolve_annotation(&mut self, annotation: &ast::TypeAnnotation) -> Type {
        match &annotation.kind {
            ast::AnnotationKind::Scalar(ty) => annotated_type_to_type(*ty),
            ast::AnnotationKind::Color => Type::Color,
            ast::AnnotationKind::Array(elem) => match self.resolve_field_type(elem, "") {
                Some(elem) => Type::Array(Box::new(elem)),
                None => Type::Error,
            },
            ast::AnnotationKind::Matrix(elem) => match self.resolve_field_type(elem, "") {
                Some(elem) => Type::Matrix(Box::new(elem)),
                None => Type::Error,
            },
            ast::AnnotationKind::Map(key_ty, value) => {
                let key = self
                    .resolve_field_type(key_ty, "")
                    .and_then(|key| self.check_map_key_type(key, key_ty.span));
                match (key, self.resolve_field_type(value, "")) {
                    (Some(key), Some(value)) => Type::Map(Box::new(key), Box::new(value)),
                    _ => Type::Error,
                }
            }
            ast::AnnotationKind::Named(name) => {
                // A seeded drawing object type or a user-defined type both
                // resolve to `Type::Object`; an enum name to `Type::Enum`.
                if let Some(resolved) = self.object_type_name(name) {
                    Type::Object(resolved)
                } else if let Some(resolved) = self.enum_type_name(name) {
                    Type::Enum(resolved)
                } else {
                    self.error(
                        codes::E0236,
                        format!("unknown type `{name}` in declaration annotation"),
                        annotation.span,
                        "expected a scalar type, `color`, `array<T>`, a drawing type, or a declared type/enum name",
                    );
                    Type::Error
                }
            }
        }
    }

    pub(crate) fn analyze_assign(
        &mut self,
        name: &ast::Ident,
        value: &ast::Expr,
        is_var: bool,
        is_varip: bool,
        annotation: Option<&ast::TypeAnnotation>,
        span: Span,
    ) -> Option<HirStmt> {
        // An explicit annotation fixes the declared type and doubles as the
        // expected type for the initializer, so `float x = na` / `int n = na`
        // type the variable (the explicit way to initialize with `na`). A named
        // annotation (`line l = na`) resolves to the matching drawing object
        // type, so `l := line.new(...)` type-checks and a wrong-type value is a
        // clear `E0226`/`E0218` mismatch.
        let annotated_ty = annotation.map(|a| self.resolve_annotation(a));
        self.in_var_initializer = is_var;
        // A `var`/`varip` initializer runs on the first bar only, but the
        // statement's pending statements run on every bar: keep its
        // arguments inline (see `arg_order.rs`).
        let value_hir = if is_var || is_varip {
            self.without_arg_order_captures(|this| {
                this.analyze_expr_expecting(value, annotated_ty.as_ref())
            })
        } else {
            self.analyze_expr_expecting(value, annotated_ty.as_ref())
        };
        self.finish_assign(
            name,
            value,
            value_hir,
            annotated_ty,
            is_var,
            is_varip,
            annotation,
            span,
        )
    }

    /// Declare the variable of an analyzed `analyze_assign` initializer.
    ///
    /// Kept out of `analyze_assign`, which is on the stack while its
    /// initializer is analyzed: an inlined method call binds its receiver
    /// through it, so a method chain recurses through it once per call.
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn finish_assign(
        &mut self,
        name: &ast::Ident,
        value: &ast::Expr,
        value_hir: HirExpr,
        annotated_ty: Option<Type>,
        is_var: bool,
        is_varip: bool,
        annotation: Option<&ast::TypeAnnotation>,
        span: Span,
    ) -> Option<HirStmt> {
        let rendering_value = self.rendering_inline.last(&value_hir);
        self.in_var_initializer = false;

        // A declaration may shadow a built-in variable; declaring in the same
        // scope twice is an error.
        self.note_builtin_shadowing(name);
        if let Some(existing) = self
            .scopes
            .last()
            .unwrap()
            .get(&name.name)
            .filter(|_| name.name != DISCARD_NAME)
        {
            let first = self.symbols[existing.0 as usize].decl_span;
            self.diagnostics.push(
                Diagnostic::error(
                    codes::E0211,
                    format!("variable `{}` is already declared in this scope", name.name),
                )
                .with_label(name.span, "redeclared here")
                .with_label(first, "first declared here")
                .with_note("to change an existing variable, reassign with `:=`"),
            );
            return None;
        }

        if value_hir.ty == Type::Void {
            self.error(
                codes::E0212,
                "cannot assign a void value",
                value.span(),
                "this expression produces no value",
            );
            return None;
        }

        // Reconcile an explicit annotation against the initializer. The
        // annotation is the declared type; the initializer must match, except
        // that a `float` annotation accepts an `int` initializer (IntToFloat).
        // Any other disagreement is E0226. A failed initializer (`Error`)
        // suppresses the mismatch cascade but the annotation still types the
        // variable.
        let (value_hir, declared_ty) = match &annotated_ty {
            Some(ann_ty) => {
                // An `Error` initializer already reported a diagnostic; a
                // matching type needs no coercion. Both keep the initializer
                // as-is and adopt the annotation as the declared type.
                if value_hir.ty.is_error() || value_hir.ty == *ann_ty {
                    (value_hir, ann_ty.clone())
                } else if *ann_ty == Type::Float && value_hir.ty == Type::Int {
                    (Self::coerce_int_to_float(value_hir), Type::Float)
                } else {
                    let ann_span = annotation.expect("annotated_ty implies annotation").span;
                    self.diagnostics.push(
                        Diagnostic::error(
                            codes::E0226,
                            format!(
                                "declared type {} does not match initializer type {}",
                                ann_ty.describe(),
                                value_hir.ty.describe()
                            ),
                        )
                        .with_label(ann_span, format!("declared as {}", ann_ty.describe()))
                        .with_label(
                            value.span(),
                            format!("initializer is {}", value_hir.ty.describe()),
                        ),
                    );
                    return None;
                }
            }
            None => {
                let ty = if value_hir.ty.is_error() {
                    Type::Error
                } else {
                    value_hir.ty.clone()
                };
                (value_hir, ty)
            }
        };

        let declared_qualifier = annotation.and_then(|a| a.qualifier).map(|q| match q {
            ast::TypeQualifier::Const => Qualifier::Const,
            ast::TypeQualifier::Simple => Qualifier::Simple,
            ast::TypeQualifier::Series => Qualifier::Series,
        });
        // Source closure is distinct from the declared qualifier. An explicit
        // Simple/Series annotation must not erase a literal initializer's proof.
        let source_closed_active_origin = self.source_closed_active_expr_is_proven(&value_hir);
        let mut value_hir = value_hir;
        if let Some(qualifier) = declared_qualifier {
            // `const` on a reference prevents rebinding, but its contents and
            // all expressions using the reference remain series-qualified.
            let const_reference = qualifier == Qualifier::Const
                && (declared_ty.is_collection() || matches!(declared_ty, Type::Object(_)));
            if !const_reference && !value_hir.ty.is_error() && value_hir.qualifier > qualifier {
                self.error(
                    codes::E0226,
                    format!(
                        "cannot initialize `{}` declared as {} with a {} value",
                        name.name,
                        qualifier.describe(),
                        value_hir.qualifier.describe()
                    ),
                    value.span(),
                    "the initializer has a stronger qualifier than the declaration",
                );
                return None;
            }
            if !const_reference {
                value_hir.qualifier = qualifier;
            }
        }

        // Persistent reads can observe a value carried from a later write in
        // source order. A lexical summary establishes that floor before any
        // consumers or aliases see the symbol. Explicit qualifier ceilings
        // and reference/plot-handle rules retain their existing treatment.
        let persistent_floor = (is_var || is_varip)
            .then(|| {
                self.persistent_write_plan
                    .floors
                    .get(&name.span)
                    .and_then(|p| p.declaration)
            })
            .flatten()
            .filter(|_| {
                declared_qualifier.is_none()
                    && matches!(
                        declared_ty,
                        Type::Int
                            | Type::Float
                            | Type::Bool
                            | Type::String
                            | Type::Color
                            | Type::Enum(_)
                    )
            });
        // An explicit ceiling constrains initialization and writes; it does
        // not make a supported carried read fixed for earlier consumers.
        let known_carried_scalar = (is_var || is_varip)
            && self
                .persistent_write_plan
                .known_carried_reads
                .contains(&name.span)
            && matches!(
                declared_ty,
                Type::Int | Type::Float | Type::Bool | Type::String | Type::Color | Type::Enum(_)
            );
        let symbol_qualifier = value_hir
            .qualifier
            .join(persistent_floor.unwrap_or(Qualifier::Const))
            .join(if known_carried_scalar {
                Qualifier::Series
            } else {
                Qualifier::Const
            });

        // An input that omits `title` takes the assigned variable name.
        // Preserve explicit `title = ""` (Some("")) exactly. Record
        // direct provenance as well so a later `active = enabled` can resolve
        // the variable read back to this input declaration.
        let input_origin = self.input_origin_for_expr(&value_hir);
        let input_active_origin = self.input_active_expr_from_hir(&value_hir);
        let request_input_expr_origin = self.request_input_expr_from_hir(&value_hir);
        let current_feed_symbol = self
            .chart_symbol_origin_for_expr(&value_hir)
            .filter(|series| {
                *series != BuiltinSeries::SyminfoBasecurrency || (!is_var && !is_varip)
            });
        // A `var` can retain a later `:=` value on the next bar, even when
        // that reassignment appears after this request in source order.
        let current_chart_timeframe =
            !is_var && !is_varip && self.current_chart_timeframe_for_expr(&value_hir);
        if let Some(origin) = input_origin {
            if let Some(decl) = self.inputs.get_mut(origin.input as usize) {
                if decl.title.is_none() {
                    decl.title = Some(name.name.clone());
                }
            }
        }

        // A compile-time-known integer initializer stamps the symbol's
        // `const_int`, so it can flow into a literal-required position (a `ta.*`
        // engine length): an int literal (directly or via another const symbol),
        // or an `input.int(...)` whose value is its default. The `simple int`
        // qualifier of a length may thus come from an input, which is fixed for
        // the run. `ta_int_inputs` separately preserves
        // its provenance so a host override can replace the default. An
        // unchanged `var` initialized from an input is fixed for the run; a
        // later `:=` clears these stamps (see `analyze_reassign`). `varip` can
        // change intrabar, while series-qualified values can vary by bar.
        let const_int = if is_varip || symbol_qualifier == Qualifier::Series {
            None
        } else {
            self.ast_const_int(value).or(match &value_hir.kind {
                HirExprKind::ReadInput { input } => match self.inputs[*input as usize].defval {
                    InputValue::Int(n) => Some(n),
                    _ => None,
                },
                _ => None,
            })
        };
        let ta_int_input = if !is_varip && symbol_qualifier != Qualifier::Series {
            self.ast_ta_input(value).or_else(|| {
                self.selected_ta_int_input(&value_hir)
                    .map(|input| (input, false))
            })
        } else {
            None
        };
        // Input metadata is fixed before bar execution. A `var` initialized
        // from a const expression still has that value unless `:=` later
        // changes it; such aliases are accepted for `group`/`inline`.
        // Reassignment clears this stamp below. Keep `varip` excluded because
        // it may change during intrabar execution.
        let const_input_value = if !is_varip && !known_carried_scalar {
            self.const_input_value_from_hir(&value_hir)
        } else {
            None
        };
        // A held display set loses how it was written, and the inline input
        // rule depends on that (see `input_display_bits`). Keep the inline
        // verdict of the initializer for const display aliases.
        let default_display_mask = if !is_var && !is_varip && !known_carried_scalar {
            self.default_display_mask(&value_hir)
        } else {
            None
        };
        let held_input_display_bits = match &const_input_value {
            Some(_) if declared_ty.is_plot_display() => self.input_display_bits(value),
            _ => None,
        };
        let const_accessor_stable =
            !is_var && !is_varip && self.const_accessor_expr_is_stable(&value_hir);
        let const_color_transparency = if !is_var && !is_varip {
            self.const_color_transparency(&value_hir)
        } else {
            None
        };
        let plot_handle = match (&value_hir.kind, &declared_ty) {
            (HirExprKind::ConstInt(plot_slot), Type::Plot | Type::Hline) => {
                u32::try_from(*plot_slot).ok().map(PlotSlot)
            }
            // `q = p` names the same plot as `p`. Reassigning either variable
            // clears its own handle (see `analyze_reassign`). A `var`/`varip`
            // source keeps a later `:=` across bars, which changes what a
            // per-bar alias reads from the next bar on, so the alias records
            // that source as a root: a `:=` of the root clears the alias, and
            // reports any `fill` that already used it.
            (HirExprKind::ReadVar { symbol, offset: 0 }, Type::Plot | Type::Hline) => {
                self.plot_handles.get(symbol).copied()
            }
            _ => None,
        };
        // A `var`/`varip` declaration runs its initializer once, on the first
        // bar, before any `:=` below it, so it keeps that first-bar handle and
        // depends on no root. A per-bar alias depends on every root of its
        // source.
        let plot_handle_roots = match &value_hir.kind {
            HirExprKind::ReadVar { symbol, offset: 0 }
                if plot_handle.is_some() && !is_var && !is_varip =>
            {
                self.plot_handle_persistent_roots(*symbol)
            }
            _ => Vec::new(),
        };
        let id = SymbolId(self.symbols.len() as u32);
        let slot = SeriesSlot(id.0);
        self.symbols.push(Symbol {
            id,
            name: name.name.clone(),
            ty: declared_ty,
            qualifier: symbol_qualifier,
            slot,
            persistent: is_var,
            varip: is_varip,
            synthetic: false,
            const_int,
            decl_span: name.span,
        });
        if name.name != DISCARD_NAME {
            self.scopes
                .last_mut()
                .unwrap()
                .insert(name.name.clone(), id);
            self.check_scope_variable_limit(&name.name, name.span);
        }
        self.assignment_declaration_depths
            .insert(id, self.assignment_controls.len());
        if let Some(qualifier) = declared_qualifier {
            self.declared_qualifiers
                .insert(id, (qualifier, self.branch_depth));
        }
        if let Some(origin) = ta_int_input {
            self.ta_int_inputs.insert(id, origin);
        }
        if let Some(plot_slot) = plot_handle {
            self.plot_handles.insert(id, plot_slot);
        }
        if !plot_handle_roots.is_empty() {
            self.plot_handle_roots.insert(id, plot_handle_roots);
        }
        if let Some(value) = const_input_value {
            self.const_input_values.insert(id, value);
        }
        if let Some(mask) = default_display_mask {
            self.default_display_masks.insert(id, mask);
        }
        if let Some(bits) = held_input_display_bits {
            self.held_input_display_bits.insert(id, bits);
        }
        if let Some(value) = const_color_transparency {
            self.const_color_transparencies.insert(id, value);
        }
        if const_accessor_stable {
            self.const_accessor_stable_symbols.insert(id);
        }
        if !known_carried_scalar {
            if let Some(origin) = input_origin {
                self.input_origins.insert(id, origin);
            }
            if let Some(expression) = input_active_origin {
                self.input_active_origins.insert(id, expression);
                if source_closed_active_origin {
                    self.source_closed_active_origins.insert(id);
                }
            }
            if let Some(expression) = request_input_expr_origin {
                self.request_input_expr_origins.insert(id, expression);
            }
            if let Some(series) = current_feed_symbol {
                self.current_feed_symbols.insert(id, series);
            }
        }
        if current_chart_timeframe {
            self.current_chart_timeframes.insert(id);
        }
        self.temporal_declaration(id, &value_hir);
        if !is_var
            && !is_varip
            && !self.in_conditional_branch()
            && self.loop_depth == 0
            && matches!(value_hir.ty, Type::Int | Type::Float)
        {
            self.htf_expr_origins.insert(id, value_hir.clone());
        }
        self.rendering_inline.attach(
            rendering_inline_provenance::Anchor::Assignment(id, span),
            &value_hir,
            rendering_value,
        );
        Some(HirStmt::Assign {
            symbol: id,
            value: value_hir,
            init: true,
            span,
        })
    }

    /// `x := value` — reassign an existing user variable in any enclosing
    /// scope. Unknown or non-assignable targets are `E0217`; type mismatches
    /// (other than the int → float widening) are `E0218`.
    /// The `var`/`varip` symbols whose reassignment would change the plot
    /// that `id` names on a later bar: `id` itself when it is persistent, and
    /// the roots it was aliased from.
    pub(crate) fn plot_handle_persistent_roots(&self, id: SymbolId) -> Vec<SymbolId> {
        let symbol = &self.symbols[id.0 as usize];
        let mut roots = self.plot_handle_roots.get(&id).cloned().unwrap_or_default();
        if (symbol.persistent || symbol.varip) && !roots.contains(&id) {
            roots.push(id);
        }
        roots
    }

    /// Drop the compile-time plot handle of a reassigned variable. When the
    /// variable is a `var`/`varip` root, the new value carries into the next
    /// bar: every per-bar alias read from it loses its handle too, and every
    /// `fill` endpoint already resolved through it is reported, because the
    /// band would otherwise keep the first-bar slot on every bar.
    fn invalidate_plot_handle(&mut self, symbol_id: SymbolId) {
        self.plot_handles.remove(&symbol_id);
        self.plot_handle_roots.remove(&symbol_id);
        let symbol = &self.symbols[symbol_id.0 as usize];
        if !(symbol.persistent || symbol.varip) {
            return;
        }
        let name = symbol.name.clone();
        let aliases: Vec<SymbolId> = self
            .plot_handle_roots
            .iter()
            .filter(|(_, roots)| roots.contains(&symbol_id))
            .map(|(alias, _)| *alias)
            .collect();
        for alias in aliases {
            self.plot_handles.remove(&alias);
            self.plot_handle_roots.remove(&alias);
        }
        let (reported, kept): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.persistent_fill_endpoints)
                .into_iter()
                .partition(|endpoint| endpoint.root == symbol_id);
        self.persistent_fill_endpoints = kept;
        for endpoint in reported {
            self.error(
                codes::E0205,
                format!(
                    "`fill` argument `{}` does not name one `{}` call",
                    endpoint.param, endpoint.call
                ),
                endpoint.span,
                format!(
                    "`{name}` is reassigned later and keeps the new value on the next bar; pass a variable that holds one `{}` result and is not reassigned",
                    endpoint.call
                ),
            );
        }
    }

    pub(crate) fn analyze_reassign(
        &mut self,
        name: &ast::Ident,
        value: &ast::Expr,
        span: Span,
    ) -> Option<HirStmt> {
        // Object field write `obj.field := value` (and chained `a.b.field := ...`):
        // the head segment names an object-typed variable. Handled before the
        // plain-variable path since a dotted target is never a plain variable.
        if let Some((head, rest)) = name.name.split_once('.') {
            if let Some(id) = self.lookup_variable(head) {
                if matches!(self.symbols[id.0 as usize].ty, Type::Object(_)) {
                    return self.analyze_field_write(id, rest, value, span);
                }
            }
        }
        // When the target resolves to a variable, its declared type is the
        // expected type — `x := na` adopts it.
        let expected = self
            .lookup_variable(&name.name)
            .map(|id| self.symbols[id.0 as usize].ty.clone());
        let value_hir = self.analyze_expr_expecting(value, expected.as_ref());
        let rendering_value = self.rendering_inline.last(&value_hir);

        // A variable may shadow a built-in series; only an unshadowed
        // built-in is read-only.
        if let Some(series) = self
            .registry
            .lookup_series(&name.name)
            .filter(|_| self.lookup_variable(&name.name).is_none())
        {
            self.error(
                codes::E0217,
                format!("cannot reassign built-in series `{}`", series.name()),
                name.span,
                "built-in series are read-only",
            );
            return None;
        }
        let Some(symbol_id) = self.lookup_variable(&name.name) else {
            if self.registry.lookup_function(&name.name).is_some() {
                self.error(
                    codes::E0217,
                    format!(
                        "cannot reassign `{}`: it is a built-in function, not a variable",
                        name.name
                    ),
                    name.span,
                    "function names are not assignable",
                );
            } else if name.name == DISCARD_NAME {
                self.error(
                    codes::E0217,
                    "cannot reassign `_`: a variable named `_` is unused by definition",
                    name.span,
                    "give the variable a name to change its value",
                );
            } else if let Some(head) = name
                .name
                .split('.')
                .next()
                .filter(|head| self.global_declared_below_function(head).is_some())
            {
                // Declared later or not, a global may not be rebound inside a
                // function; report that rather than "undeclared".
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::E0217,
                        format!("cannot reassign global variable `{head}` inside a function"),
                    )
                    .with_label(
                        name.span,
                        "this binding belongs to a scope outside the current function",
                    )
                    .with_note(crate::exprs::declared_below_note(head)),
                );
            } else {
                self.error(
                    codes::E0217,
                    format!(
                        "cannot reassign undeclared variable `{}`; declare it first with `=`",
                        name.name
                    ),
                    name.span,
                    "not declared in any enclosing scope",
                );
            }
            return None;
        };

        if let Some(scope_base) = self.function_scope_bases.last().map(|b| b.base) {
            // Lookup reaches only the function's own scopes and the global
            // scope, so a binding below the base is a global.
            let owned_binding = self
                .scopes
                .iter()
                .skip(scope_base)
                .any(|scope| scope.get(&name.name).is_some_and(|id| *id == symbol_id));
            if !owned_binding {
                self.error(
                    codes::E0217,
                    format!(
                        "cannot reassign global variable `{}` inside a function",
                        name.name
                    ),
                    name.span,
                    "this binding belongs to a scope outside the current function",
                );
                return None;
            }
            if self.function_parameters.contains(&symbol_id) {
                self.error(
                    codes::E0217,
                    format!("cannot reassign function parameter `{}`", name.name),
                    name.span,
                    "a function may not reassign its parameters; assign a local copy",
                );
                return None;
            }
        }

        let control_depth = self
            .assignment_declaration_depths
            .get(&symbol_id)
            .copied()
            .unwrap_or(0);
        let control_qualifier = self
            .assignment_controls
            .iter()
            .skip(control_depth)
            .copied()
            .fold(Qualifier::Const, Qualifier::join);
        if let Some((qualifier, declared_depth)) = self.declared_qualifiers.get(&symbol_id).copied()
        {
            if qualifier == Qualifier::Const {
                self.error(
                    codes::E0217,
                    format!("cannot reassign const variable `{}`", name.name),
                    name.span,
                    "const declarations cannot be rebound",
                );
                return None;
            }
            // A write in a block nested below the declaration makes the
            // variable series, so check the qualifier it would end with.
            let written = if self.branch_depth > declared_depth {
                value_hir.qualifier.join(Qualifier::Series)
            } else {
                value_hir.qualifier
            }
            .join(control_qualifier);
            if written > qualifier {
                self.error(
                    codes::E0218,
                    format!(
                        "cannot assign a {} value to {} variable `{}`",
                        written.describe(),
                        qualifier.describe(),
                        name.name
                    ),
                    value.span(),
                    "the assigned value has a stronger qualifier than the declaration",
                );
                return None;
            }
        }

        // The loop counter is immutable inside its own body: a `:=` that
        // resolves to a loop counter is `E0217`.
        if self.loop_counters.contains(&symbol_id) {
            self.error(
                codes::E0217,
                format!("cannot reassign the loop counter `{}`", name.name),
                name.span,
                "the loop controls this variable",
            );
            return None;
        }

        let symbol_ty = self.symbols[symbol_id.0 as usize].ty.clone();
        // Cascade suppression: a failed sub-expression (or a declaration that
        // failed to type) already produced a diagnostic.
        if value_hir.ty.is_error() || symbol_ty.is_error() {
            return None;
        }

        let widens_display =
            symbol_ty == Type::PlotSimpleDisplay && value_hir.ty == Type::PlotDisplay;
        let value_hir = if value_hir.ty == symbol_ty
            || widens_display
            || (symbol_ty == Type::PlotDisplay && value_hir.ty == Type::PlotSimpleDisplay)
        {
            value_hir
        } else if symbol_ty == Type::Float && value_hir.ty == Type::Int {
            // The one permitted widening: an int value into a float variable.
            Self::coerce_int_to_float(value_hir)
        } else {
            self.error(
                codes::E0218,
                format!(
                    "cannot reassign `{}`: it has type {}, but the value is {}",
                    name.name,
                    symbol_ty.describe(),
                    value_hir.ty.describe()
                ),
                value.span(),
                format!("expected {}", symbol_ty.describe()),
            );
            return None;
        };

        // Join the assigned value and the conditions that control the write.
        // Input/simple conditions are run-stable but still exclude `const`.
        // Series-controlled writes remain time-dependent. Persistent slots
        // use their measured lexical floor: a stable write-only prefix can be
        // simple, while carried/unknown flows retain the Series safeguard.
        let in_branch_scope = self.in_conditional_branch();
        let persistence_floor = {
            let symbol = &self.symbols[symbol_id.0 as usize];
            (symbol.persistent || symbol.varip).then(|| {
                self.persistent_write_plan
                    .floors
                    .get(&symbol.decl_span)
                    .map(|p| p.assignment)
                    .filter(|_| {
                        !self.declared_qualifiers.contains_key(&symbol_id)
                            && matches!(
                                symbol.ty,
                                Type::Int
                                    | Type::Float
                                    | Type::Bool
                                    | Type::String
                                    | Type::Color
                                    | Type::Enum(_)
                            )
                    })
                    .unwrap_or(Qualifier::Series)
            })
        };
        let symbol = &mut self.symbols[symbol_id.0 as usize];
        // Subsequent reads can observe either display value.
        if widens_display {
            symbol.ty = Type::PlotDisplay;
        }
        symbol.qualifier = symbol
            .qualifier
            .join(value_hir.qualifier)
            .join(control_qualifier);
        if in_branch_scope {
            symbol.qualifier = symbol.qualifier.join(Qualifier::Series);
        }
        if let Some(floor) = persistence_floor {
            symbol.qualifier = symbol.qualifier.join(floor);
        }
        // A reassigned variable no longer carries a known compile-time value.
        symbol.const_int = None;
        self.ta_int_inputs.remove(&symbol_id);
        self.const_input_values.remove(&symbol_id);
        self.held_input_display_bits.remove(&symbol_id);
        self.default_display_masks.remove(&symbol_id);
        self.const_color_transparencies.remove(&symbol_id);
        self.const_accessor_stable_symbols.remove(&symbol_id);
        self.input_origins.remove(&symbol_id);
        self.input_active_origins.remove(&symbol_id);
        self.source_closed_active_origins.remove(&symbol_id);
        self.request_input_expr_origins.remove(&symbol_id);
        self.htf_expr_origins.remove(&symbol_id);
        self.current_feed_symbols.remove(&symbol_id);
        self.current_chart_timeframes.remove(&symbol_id);
        self.invalidate_plot_handle(symbol_id);

        self.temporal_reassignment(symbol_id, &value_hir);
        self.rendering_inline.attach(
            rendering_inline_provenance::Anchor::Assignment(symbol_id, span),
            &value_hir,
            rendering_value,
        );
        Some(HirStmt::Assign {
            symbol: symbol_id,
            value: value_hir,
            init: false,
            span,
        })
    }

    /// `[a, b, c] = expr` — a destructuring declaration. The RHS must be a
    /// tuple-producing call: a tuple-returning `ta.*` function
    /// (`ta.macd`/`ta.bb`/`ta.kc`/`ta.dmi`) or a user function whose body ends
    /// in a tuple expression. The number of targets must match the tuple's
    /// arity (`E0225`); each target is declared afresh (`E0211` on a
    /// redeclaration or built-in collision).
    ///
    /// Two lowering shapes:
    /// - A tuple `ta.*` call lowers to **one** [`HirStmt::Destructure`] over a
    ///   single tuple-returning [`HirExprKind::CallTaState`]; MIR emits one
    ///   multi-push `CALL_TA` and N stores.
    /// - A user tuple function's result is N independent element expressions
    ///   (bound to the call site's result slots), so each target becomes an
    ///   ordinary [`HirStmt::Assign`] declaration — no multi-push needed.
    pub(crate) fn analyze_destructure(
        &mut self,
        targets: &[ast::Ident],
        value: &ast::Expr,
        span: Span,
    ) -> Option<HirStmt> {
        // Resolve the RHS to a tuple. A direct call to a tuple `ta.*` function
        // becomes a multi-push CallTaState; a user-function call is expanded
        // and must yield a `Tuple`; anything else is an arity error (E0225).
        match self.analyze_tuple_rhs(value, targets.len()) {
            TupleRhs::TaCall {
                call,
                elem_ty,
                arity,
            } => {
                // Declare every target up front (with the tuple's element type)
                // so a later reference does not cascade `E0201`, even on an
                // arity mismatch.
                let target_ids: Vec<SymbolId> = targets
                    .iter()
                    .map(|t| self.declare_target(t, elem_ty.clone(), Qualifier::Series))
                    .collect();
                if !self.check_tuple_arity(targets, arity, value.span()) {
                    return None;
                }
                Some(HirStmt::Destructure {
                    targets: target_ids,
                    value: call,
                    span,
                })
            }
            TupleRhs::Elements(elements) => {
                // Declare each target with its matching element's type (or
                // `Error` when the pattern is longer than the tuple), again to
                // suppress an `E0201` cascade on a mismatch.
                let target_ids: Vec<SymbolId> = targets
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let (ty, qualifier) = elements
                            .get(i)
                            .map_or((Type::Error, Qualifier::Series), |e| {
                                (e.ty.clone(), e.qualifier)
                            });
                        self.declare_target(t, ty, qualifier)
                    })
                    .collect();
                if !self.check_tuple_arity(targets, elements.len() as u32, value.span()) {
                    return None;
                }
                // Retain the source tuple origin for OHLC presentation metadata.
                // The actual element expressions, qualifiers, symbols and stores
                // remain unchanged; ordinary `=` declarations never enter here.
                self.ohlc_destructure_targets
                    .extend(target_ids.iter().copied());
                // Each element is an independent value: emit an ordinary
                // declaration store per target. The pending block (the expanded
                // function body) already ran before this statement.
                for (id, element) in target_ids.into_iter().zip(elements) {
                    self.pending_stmts.push(HirStmt::Assign {
                        symbol: id,
                        value: element,
                        init: true,
                        span,
                    });
                }
                // The declarations were pushed as pending statements (so they
                // run in order after the function body); there is no single
                // trailing HIR statement.
                None
            }
            TupleRhs::Error => {
                // The RHS failed; still declare the targets as error
                // placeholders so later references do not cascade.
                for t in targets {
                    let _ = self.declare_target(t, Type::Error, Qualifier::Series);
                }
                None
            }
        }
    }

    /// A call on the right-hand side of a destructuring declaration that
    /// produces the tuple itself (`request.security`, a tuple `ta.*`
    /// function, a user function or method), or `None` for any other call.
    fn analyze_tuple_call_rhs(
        &mut self,
        callee: &ast::Ident,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<TupleRhs> {
        if callee.name == "request.security" {
            return Some(self.analyze_security_tuple_call(args, span));
        }
        if callee.name == "request.security_lower_tf" {
            if let Some((position, values)) = args.iter().enumerate().find_map(|(position, arg)| {
                let expression = arg
                    .name
                    .as_ref()
                    .map_or(position == 2, |name| name.name == "expression");
                if expression {
                    if let ast::Expr::Tuple { elements, .. } = &arg.value {
                        return Some((position, elements));
                    }
                }
                None
            }) {
                let elements = values
                    .iter()
                    .map(|value| {
                        let mut scalar = args.to_vec();
                        scalar[position].value = value.clone();
                        self.analyze_security_lower_tf_call(&scalar, span)
                    })
                    .collect();
                return Some(TupleRhs::Elements(elements));
            }
        }
        if callee.name == "ta.vwap" && args.len() == 3 {
            return Some(self.analyze_ta_vwap_tuple_call(args, span));
        }
        // A tuple-returning `ta.*` function: one multi-push CallTaState.
        if let Some(func) = self.registry.lookup_function(&callee.name) {
            if let BuiltinFunctionKind::Ta(ta) = func.kind {
                // `ta.vwap` also routes here: its banded 3-arg form
                // (`TaFunc::VwapBands`) is tuple-returning, resolved by
                // arity inside `analyze_ta_tuple_call`.
                if ta.is_tuple() || ta == TaFunc::Vwap {
                    return Some(self.analyze_ta_tuple_call(func, ta, args, span));
                }
            }
        }
        // A user function (or an imported `alias.fn(...)`): expand it.
        // Expansion permits a tuple body-tail, so the result is a `Tuple`
        // when the body ends in one.
        let expanded = if let Some((alias, fname)) = callee.name.split_once('.') {
            self.resolve_alias(alias)
                .map(|unit| self.expand_lib_call(unit, alias, fname, callee, args, span))
        } else if self.current_functions().contains_key(&callee.name) {
            Some(self.expand_function_call(callee, args, span))
        } else {
            // A method called as a function (`[a, b] = m(x)`).
            self.try_method_as_function(callee, args, span)
        };
        if let Some(result) = expanded {
            if result.ty.is_error() {
                return Some(TupleRhs::Error);
            }
            return Some(match result.kind {
                HirExprKind::Tuple { elements } => TupleRhs::Elements(elements),
                // A scalar-returning function: a one-tuple (arity check
                // reports the mismatch unless the pattern has one target).
                _ => TupleRhs::Elements(vec![result]),
            });
        }
        None
    }

    /// Resolve the RHS of a destructuring declaration to a tuple. A call to a
    /// tuple `ta.*` function becomes a single multi-push `CallTaState`; a call
    /// to a user function is expanded (its body-tail tuple supplies the
    /// element expressions); any other (scalar) expression yields a one-element
    /// result so the arity check reports the mismatch (`E0225`).
    pub(crate) fn analyze_tuple_rhs(&mut self, value: &ast::Expr, arity_hint: usize) -> TupleRhs {
        // Unwrap parentheses: `[a, b] = (ta.bb(...))` destructures the call.
        let mut inner = value;
        while let ast::Expr::Group { inner: i, .. } = inner {
            inner = i;
        }

        if let ast::Expr::Call {
            callee, args, span, ..
        } = inner
        {
            // The statement analyzer dispatches these calls itself, so give
            // them the argument-order frame (or the inline suppression) the
            // expression path installs for the same call.
            if let Some(rhs) = self.with_call_arg_order(inner, |this| {
                this.analyze_tuple_call_rhs(callee, args, *span)
            }) {
                return rhs;
            }
        }

        // Any other RHS is a scalar value: analyze it and let the arity check
        // report the mismatch. A void value (an effect call) is E0212.
        let hir = self.analyze_expr(value);
        if hir.ty.is_error() {
            return TupleRhs::Error;
        }
        if let HirExprKind::Tuple { elements } = hir.kind {
            return TupleRhs::Elements(elements);
        }
        if hir.ty == Type::Void {
            self.error(
                codes::E0212,
                "cannot destructure a void value",
                value.span(),
                "this expression produces no value",
            );
            return TupleRhs::Error;
        }
        let _ = arity_hint;
        TupleRhs::Elements(vec![hir])
    }

    /// Check that a destructuring pattern's target count matches the tuple
    /// arity; report `E0225` and return false on a mismatch.
    pub(crate) fn check_tuple_arity(
        &mut self,
        targets: &[ast::Ident],
        arity: u32,
        span: Span,
    ) -> bool {
        if targets.len() as u32 == arity {
            return true;
        }
        let pattern_span = targets
            .first()
            .map_or(span, |t| t.span.to(targets.last().unwrap().span));
        self.diagnostics.push(
            Diagnostic::error(
                codes::E0225,
                format!(
                    "destructuring pattern has {} target(s), but the value is a {}-tuple",
                    targets.len(),
                    arity
                ),
            )
            .with_label(pattern_span, format!("{} target(s) here", targets.len()))
            .with_label(span, format!("produces a {arity}-tuple")),
        );
        false
    }

    /// A variable may shadow a built-in variable, after which the name refers
    /// to the variable for the rest of its scope, and the compiler warns. A script that also uses the name for the built-in, in any
    /// scope, is rejected once the whole script has been analyzed
    /// (`check_builtin_shadowing`).
    pub(crate) fn note_builtin_shadowing(&mut self, name: &ast::Ident) {
        if self.registry.lookup_series(&name.name).is_none() || !self.lib_ctx.is_empty() {
            return;
        }
        self.diagnostics.push(
            Diagnostic::warning(
                codes::W0006,
                format!("this variable shadows the built-in `{}`", name.name),
            )
            .with_label(name.span, "declared here")
            .with_note(format!(
                "`{}` refers to this variable for the rest of its scope, and the script cannot also use the built-in",
                name.name
            )),
        );
        self.shadowed_builtins
            .entry(name.name.clone())
            .or_insert(name.span);
    }

    /// Reject each built-in variable that the script both shadows and uses.
    /// A script may shadow a built-in variable only when it never uses that
    /// name for the built-in anywhere.
    pub(crate) fn check_builtin_shadowing(&mut self) {
        let mut conflicts: Vec<(Span, Span, String)> = self
            .shadowed_builtins
            .iter()
            .filter_map(|(name, declared)| {
                self.builtin_reads
                    .get(name)
                    .map(|used| (*declared, *used, name.clone()))
            })
            .collect();
        conflicts.sort_by_key(|(declared, ..)| (declared.start, declared.end));
        for (declared, used, name) in conflicts {
            self.diagnostics.push(
                Diagnostic::error(
                    codes::E0211,
                    format!("cannot declare `{name}`: the script also uses the built-in `{name}`"),
                )
                .with_label(declared, "this declaration shadows the built-in")
                .with_label(used, "the built-in is used here"),
            );
        }
    }

    /// Declare one destructuring target as a fresh variable of the given type
    /// and qualifier (the same rules as `=`: `E0211` on a redeclaration or a
    /// built-in collision, after which a placeholder symbol is still returned
    /// so the caller can proceed). Returns the new symbol id.
    pub(crate) fn declare_target(
        &mut self,
        name: &ast::Ident,
        ty: Type,
        qualifier: Qualifier,
    ) -> SymbolId {
        self.note_builtin_shadowing(name);
        if let Some(existing) = self
            .scopes
            .last()
            .unwrap()
            .get(&name.name)
            .copied()
            .filter(|_| name.name != DISCARD_NAME)
        {
            let first = self.symbols[existing.0 as usize].decl_span;
            self.diagnostics.push(
                Diagnostic::error(
                    codes::E0211,
                    format!("variable `{}` is already declared in this scope", name.name),
                )
                .with_label(name.span, "redeclared here")
                .with_label(first, "first declared here")
                .with_note("to change an existing variable, reassign with `:=`"),
            );
        }
        let id = SymbolId(self.symbols.len() as u32);
        let slot = SeriesSlot(id.0);
        self.symbols.push(Symbol {
            id,
            name: name.name.clone(),
            ty,
            qualifier,
            slot,
            persistent: false,
            varip: false,
            synthetic: false,
            const_int: None,
            decl_span: name.span,
        });
        if name.name != DISCARD_NAME {
            self.scopes
                .last_mut()
                .unwrap()
                .insert(name.name.clone(), id);
            self.check_scope_variable_limit(&name.name, name.span);
        }
        self.assignment_declaration_depths
            .insert(id, self.assignment_controls.len());
        id
    }

    pub(crate) fn analyze_expr_stmt(
        &mut self,
        expr: &ast::Expr,
        span: Span,
        is_block_tail: bool,
    ) -> Option<HirStmt> {
        // Effect calls in statement position become effect statements.
        if let ast::Expr::Call {
            callee,
            args,
            span: call_span,
            ..
        } = expr
        {
            if let Some(func) = self.registry.lookup_function(&callee.name) {
                match func.kind {
                    BuiltinFunctionKind::Plot => {
                        if self.in_local_scope() {
                            self.error(
                                codes::E0254,
                                format!("Cannot use \"{}\" in local scope", func.name),
                                *call_span,
                                "plot-family calls are only allowed in global scope",
                            );
                            for arg in args {
                                let _ = self.analyze_expr(&arg.value);
                            }
                            return None;
                        }
                        return self.with_ordered_call_args(args, |this| {
                            this.analyze_plot_call(callee, args, *call_span)
                        });
                    }
                    BuiltinFunctionKind::Indicator => {
                        self.error(
                            codes::E0214,
                            "`indicator(...)` must appear at the top level of the script",
                            *call_span,
                            "not allowed inside blocks",
                        );
                        return None;
                    }
                    BuiltinFunctionKind::MaxBarsBack => {
                        self.analyze_max_bars_back_call(callee, args, *call_span);
                        return None;
                    }
                    BuiltinFunctionKind::Log(level) => {
                        return self.with_ordered_call_args(args, |this| {
                            this.analyze_log_call(level, args, *call_span)
                        });
                    }
                    BuiltinFunctionKind::Alert => {
                        return self.with_ordered_call_args(args, |this| {
                            this.analyze_alert_call(args, *call_span)
                        });
                    }
                    BuiltinFunctionKind::AlertCondition => {
                        // Global-only like the plot family.
                        if self.in_local_scope() {
                            self.error(
                                codes::E0254,
                                format!("Cannot use \"{}\" in local scope", func.name),
                                *call_span,
                                "alertcondition() is only allowed in global scope",
                            );
                            for arg in args {
                                let _ = self.analyze_expr(&arg.value);
                            }
                            return None;
                        }
                        return self.with_ordered_call_args(args, |this| {
                            this.analyze_alertcondition_call(args, *call_span)
                        });
                    }
                    BuiltinFunctionKind::Display(kind) => {
                        if self.in_local_scope() {
                            self.error(
                                codes::E0254,
                                format!("Cannot use \"{}\" in local scope", func.name),
                                *call_span,
                                "plot-family calls are only allowed in global scope",
                            );
                            for arg in args {
                                let _ = self.analyze_expr(&arg.value);
                            }
                            return None;
                        }
                        return self.with_ordered_call_args(args, |this| {
                            this.analyze_display_call(kind, args, *call_span)
                        });
                    }
                    BuiltinFunctionKind::RuntimeErrorFn => {
                        return self.with_ordered_call_args(args, |this| {
                            this.analyze_runtime_error_call(args, *call_span)
                        });
                    }
                    BuiltinFunctionKind::Order(kind) => {
                        return self.with_ordered_call_args(args, |this| {
                            this.analyze_order_call(kind, callee, args, *call_span)
                        });
                    }
                    // Value-producing calls fall through to the normal
                    // expression path (and its W0002 discard warning).
                    BuiltinFunctionKind::Ta(_)
                    | BuiltinFunctionKind::Pure(_)
                    | BuiltinFunctionKind::Collection(_)
                    | BuiltinFunctionKind::MapConstructor { .. }
                    | BuiltinFunctionKind::MatrixConstructor { .. }
                    | BuiltinFunctionKind::Security
                    | BuiltinFunctionKind::Request(_)
                    | BuiltinFunctionKind::SecurityLowerTf
                    | BuiltinFunctionKind::PivotPointLevels
                    | BuiltinFunctionKind::TimeFunction { .. }
                    | BuiltinFunctionKind::Input(_)
                    | BuiltinFunctionKind::InputGeneric => {}
                }
            }
        }

        // The statement's own call, seen through redundant parentheses, runs
        // for its effects only; its result need not be a value.
        let mut invoked = expr;
        while let ast::Expr::Group { inner, .. } = invoked {
            invoked = inner;
        }
        let discarded_call = matches!(
            invoked,
            ast::Expr::Call { .. } | ast::Expr::MethodCall { .. }
        )
        .then_some(invoked.span());
        let saved_discard = std::mem::replace(&mut self.discarded_call, discarded_call);
        let mut expr_hir = self.analyze_expr(expr);
        self.discarded_call = saved_discard;
        // A tuple can only be destructured (`[a, b] = f(...)`). A method,
        // library function or helper tail that returns one, written as a
        // statement, is E0225 like a user function used as a value.
        if matches!(expr_hir.kind, HirExprKind::Tuple { .. }) {
            self.diagnostics.push(
                Diagnostic::error(
                    codes::E0225,
                    "this call returns a tuple; destructure it with `[...] = ...`",
                )
                .with_label(span, "a tuple result cannot stand alone as a statement"),
            );
            expr_hir = self.error_expr(span);
        }
        // PineWASM lets value-returning calls whose primary purpose is their
        // side effect stand alone. The returned array element or drawing id is
        // optional to consume, so warning on these would flag valid code.
        let ignored_result_is_intentional = matches!(
            &expr_hir.kind,
            HirExprKind::RegisterDrawing { .. }
                | HirExprKind::CallCollection {
                    func: CollectionFunc::ArrayPop
                        | CollectionFunc::ArrayShift
                        | CollectionFunc::ArrayRemove
                        | CollectionFunc::MatrixRemoveRow
                        | CollectionFunc::MatrixRemoveCol,
                    ..
                }
        );
        if !expr_hir.ty.is_error()
            && expr_hir.ty != Type::Void
            && !ignored_result_is_intentional
            && !is_block_tail
        {
            self.warning(
                codes::W0002,
                "expression result is discarded",
                span,
                "this value is computed and thrown away",
            );
        }
        Some(HirStmt::Eval {
            expr: expr_hir,
            span,
        })
    }

    /// Analyze a top-level `max_bars_back(series, length)` call. It produces no
    /// executable statement: its job is to (a) analyze the `series` argument
    /// so its own errors surface and (b) raise the program-wide history depth
    /// with its const-int `length`. The ceiling is settled once the whole
    /// program is analyzed (`check_max_bars_back_offsets`), and both execution
    /// tiers use the resulting metadata to size bounded user-slot history
    /// rings. Not allowed inside a function body (it is a script-global
    /// setting), mirroring `plot`.
    pub(crate) fn analyze_max_bars_back_call(
        &mut self,
        callee: &ast::Ident,
        args: &[ast::CallArg],
        span: Span,
    ) {
        if !self.call_stack.is_empty() {
            self.error(
                codes::E0214,
                "`max_bars_back(...)` is not allowed inside a function body",
                span,
                "call `max_bars_back` at the top level of the script",
            );
            for arg in args {
                let _ = self.analyze_expr(&arg.value);
            }
            return;
        }
        let func = self
            .registry
            .lookup_function(&callee.name)
            .expect("caller checked");
        let param_names: Vec<&'static str> = func.params.iter().map(|p| p.name).collect();
        let Some(bound) = self.bind_positional(func.name, &param_names, args, callee.span) else {
            return;
        };
        // Type-check both arguments (surfaces a bad `series` argument, arity
        // errors, etc.) before reading the length.
        let Some(_checked) = self.check_call_args(func, &bound, callee.span, span) else {
            return;
        };
        if let Some(length_arg) = bound.get(1) {
            if let Some((length, _)) = self.ta_const_int(
                "max_bars_back",
                "num",
                length_arg,
                1..=MAX_BARS_BACK_LIMIT as i64,
                false,
            ) {
                self.raise_max_bars_back(length as u32);
            }
        }
    }
}

impl<'r> Analyzer<'r> {
    /// Lower activation to an ordinary persistent slot. The slot is restored
    /// by realtime rollback, while the body's `varip` and effects retain
    /// their existing behavior. Guard condition prefixes as well as the body:
    /// an inactive structure must not evaluate a side-effecting condition.
    pub(crate) fn analyze_once(&mut self, once: &ast::OnceStmt) -> Option<HirStmt> {
        let flag = self.allocate_synthetic_slot(Type::Bool, once.span);
        let flag_symbol = SymbolId(flag.0);
        self.symbols[flag.0 as usize].persistent = true;

        let saved_pending = std::mem::take(&mut self.pending_stmts);
        self.assignment_controls.push(Qualifier::Series);
        self.branch_depth += 1;
        self.request_branch_depth += 1;
        let cond = match &once.cond {
            Some(cond) => self.analyze_expr_expecting(cond, Some(&Type::Bool)),
            None => HirExpr {
                kind: HirExprKind::ConstBool(true),
                ty: Type::Bool,
                qualifier: Qualifier::Const,
                span: once.span,
            },
        };
        self.request_branch_depth -= 1;
        self.branch_depth -= 1;
        self.assignment_controls.pop();
        let mut guarded_condition = std::mem::take(&mut self.pending_stmts);
        self.pending_stmts = saved_pending;
        if !cond.ty.is_error() && cond.ty != Type::Bool {
            self.error(
                codes::E0208,
                format!(
                    "`once` condition must be bool, found {}",
                    cond.ty.describe()
                ),
                once.cond.as_ref().map_or(once.span, ast::Expr::span),
                "expected a boolean expression",
            );
        }
        let mut body = self.analyze_block(&once.body, Qualifier::Series, true);
        // Set activation before the body so `break`/`continue` cannot bypass
        // it. Errors terminate the session; open ticks restore this store.
        body.insert(
            0,
            HirStmt::Assign {
                symbol: flag_symbol,
                value: HirExpr {
                    kind: HirExprKind::ConstBool(true),
                    ty: Type::Bool,
                    qualifier: Qualifier::Const,
                    span: once.span,
                },
                init: false,
                span: once.span,
            },
        );
        guarded_condition.push(HirStmt::If {
            cond,
            then_branch: body,
            else_branch: Vec::new(),
            span: once.span,
        });
        self.pending_stmts.push(HirStmt::Assign {
            symbol: flag_symbol,
            value: HirExpr {
                kind: HirExprKind::ConstBool(false),
                ty: Type::Bool,
                qualifier: Qualifier::Const,
                span: once.span,
            },
            init: true,
            span: once.span,
        });
        Some(HirStmt::If {
            cond: HirExpr {
                kind: HirExprKind::Unary {
                    op: UnOp::Not,
                    operand: Box::new(HirExpr {
                        kind: HirExprKind::ReadVar {
                            symbol: flag_symbol,
                            offset: 0,
                        },
                        ty: Type::Bool,
                        qualifier: Qualifier::Series,
                        span: once.span,
                    }),
                },
                ty: Type::Bool,
                qualifier: Qualifier::Series,
                span: once.span,
            },
            then_branch: guarded_condition,
            else_branch: Vec::new(),
            span: once.span,
        })
    }

    pub(crate) fn analyze_if(&mut self, if_stmt: &ast::IfStmt) -> Option<HirStmt> {
        let cond = self.analyze_expr(&if_stmt.cond);
        let rendering_condition = self.rendering_inline.last(&cond);
        if !cond.ty.is_error() && cond.ty != Type::Bool {
            self.error(
                codes::E0208,
                format!("`if` condition must be bool, found {}", cond.ty.describe()),
                if_stmt.cond.span(),
                "expected a boolean expression",
            );
        }

        // Input/simple conditions are fixed for an entire historical run. A
        // selected branch therefore executes on every bar (or never), so its
        // stateful calls do not have the intermittent gaps W0004 describes.
        let control_qualifier = cond.qualifier;
        let request_control = self.unresolved_request_control(&cond);
        let then_branch =
            self.analyze_block(&if_stmt.then_branch, control_qualifier, request_control);
        let else_branch = match &if_stmt.else_branch {
            Some(stmts) => {
                // `else if` is stored as an else block holding one `if`. The
                // parser bounds that chain by its block-nesting cap, as it
                // always has; the link itself is not charged again here, so a
                // flat chain of arms keeps the limit it had before the budget.
                self.else_if_link = matches!(stmts.as_slice(), [ast::Stmt::If(_)]);
                self.analyze_block(stmts, control_qualifier, request_control)
            }
            None => Vec::new(),
        };
        self.rendering_inline.attach(
            rendering_inline_provenance::Anchor::Condition(if_stmt.span),
            &cond,
            rendering_condition,
        );
        Some(HirStmt::If {
            cond,
            then_branch,
            else_branch,
            span: if_stmt.span,
        })
    }

    /// The condition series for one `switch` arm, plus a flag marking the
    /// `default` arm (which has no condition and always matches).
    pub(crate) fn switch_arm_condition(
        &mut self,
        subject_slot: Option<(SeriesSlot, Type, Qualifier)>,
        arm: &ast::SwitchArm,
        seen_default: &mut bool,
    ) -> Option<HirExpr> {
        if arm.label.is_some() && *seen_default {
            self.error(
                codes::E0225,
                "a `switch` default arm must be last",
                arm.span,
                "move every labeled arm before the default arm",
            );
        }
        match (&arm.label, subject_slot) {
            // The `default` arm: no condition. Duplicate defaults are E0225.
            (None, _) => {
                if *seen_default {
                    self.error(
                        codes::E0225,
                        "a `switch` has at most one `default` arm",
                        arm.span,
                        "duplicate default arm",
                    );
                    return None;
                }
                *seen_default = true;
                None
            }
            // Subject form: compare the once-evaluated subject to the label.
            (Some(label), Some((slot, subject_ty, subject_qualifier))) => {
                let label_hir = self.analyze_expr_expecting(label, Some(&subject_ty));
                if label_hir.ty.is_error() {
                    return None;
                }
                let condition_qualifier = subject_qualifier.join(label_hir.qualifier);
                // The label must be comparable to the subject: same type, or
                // an int/float pair (widened). Anything else is E0207.
                let read = HirExpr {
                    kind: HirExprKind::ReadVar {
                        symbol: SymbolId(slot.0),
                        offset: 0,
                    },
                    ty: subject_ty.clone(),
                    qualifier: subject_qualifier,
                    span: label.span(),
                };
                let (lhs, rhs) = if subject_ty == label_hir.ty {
                    (read, label_hir)
                } else if is_numeric(&subject_ty) && is_numeric(&label_hir.ty) {
                    (
                        Self::coerce_int_to_float(read),
                        Self::coerce_int_to_float(label_hir),
                    )
                } else {
                    self.error(
                        codes::E0207,
                        format!(
                            "`switch` case label type {} is not comparable to the subject type {}",
                            label_hir.ty.describe(),
                            subject_ty.describe()
                        ),
                        label.span(),
                        "the label must match the subject's type",
                    );
                    return None;
                };
                Some(HirExpr {
                    kind: HirExprKind::Binary {
                        op: BinOp::Eq,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    },
                    ty: Type::Bool,
                    qualifier: condition_qualifier,
                    span: label.span(),
                })
            }
            // Subjectless form: the label is a boolean condition.
            (Some(label), None) => {
                let cond = self.analyze_expr_expecting(label, Some(&Type::Bool));
                if cond.ty.is_error() {
                    return None;
                }
                if cond.ty != Type::Bool {
                    self.error(
                        codes::E0208,
                        format!(
                            "a subjectless `switch` case must be bool, found {}",
                            cond.ty.describe()
                        ),
                        label.span(),
                        "expected a boolean condition",
                    );
                    return None;
                }
                Some(cond)
            }
        }
    }

    /// Evaluate a `switch` subject once into a hidden slot, storing it via a
    /// `pending` statement so the store precedes the desugared if-chain.
    /// Returns the slot, subject type, and original qualifier, or `None` on a
    /// failed/void subject. Retaining the qualifier lets switch arms distinguish
    /// a run-stable input/simple selector from a genuinely per-bar selector.
    pub(crate) fn eval_switch_subject(
        &mut self,
        subject: &ast::Expr,
    ) -> Option<(SeriesSlot, Type, Qualifier)> {
        let subject_hir = self.analyze_expr(subject);
        if subject_hir.ty.is_error() {
            return None;
        }
        if subject_hir.ty == Type::Void {
            self.error(
                codes::E0225,
                "a `switch` subject must produce a value",
                subject.span(),
                "this expression produces no value",
            );
            return None;
        }
        let ty = subject_hir.ty.clone();
        let qualifier = subject_hir.qualifier;
        let source_closed_active_origin = self.source_closed_active_expr_is_proven(&subject_hir);
        let slot = self.allocate_synthetic_slot(ty.clone(), subject.span());
        // Capturing a switch subject does not strengthen its qualifier. The
        // hidden slot merely guarantees single evaluation; an input/simple
        // subject remains input/simple for downstream compatibility checks.
        self.symbols[slot.0 as usize].qualifier = qualifier;
        // Preserve the existing input-stable/input-free Simple host binding.
        // Source closure is recorded separately, since a host context value
        // can have that same binding shape. Capture still evaluates once.
        let origin = if qualifier <= Qualifier::Input {
            self.input_active_expr_from_hir(&subject_hir)
        } else if qualifier == Qualifier::Simple {
            self.input_free_active_expr_from_hir(&subject_hir)
        } else {
            None
        };
        if let Some(origin) = origin {
            self.input_active_origins.insert(SymbolId(slot.0), origin);
            if source_closed_active_origin {
                self.source_closed_active_origins.insert(SymbolId(slot.0));
            }
        }
        if let Some(constant) = self.const_input_value_from_hir(&subject_hir) {
            self.const_input_values.insert(SymbolId(slot.0), constant);
        }
        self.temporal_binding(SymbolId(slot.0), &subject_hir);
        self.pending_stmts.push(HirStmt::Assign {
            symbol: SymbolId(slot.0),
            value: subject_hir,
            init: false,
            span: subject.span(),
        });
        Some((slot, ty, qualifier))
    }

    /// A switch with no labeled arms executes its fallback unconditionally.
    /// Its subject is still evaluated, but does not control writes or make
    /// stateful calls intermittent. Labeled arms join their actual condition
    /// qualifiers, starting with the subject's qualifier when one is present.
    pub(crate) fn initial_switch_control_qualifier(
        switch: &ast::SwitchStmt,
        subject_qualifier: Option<Qualifier>,
    ) -> Qualifier {
        if switch.arms.iter().any(|arm| arm.label.is_some()) {
            subject_qualifier.unwrap_or(Qualifier::Const)
        } else {
            Qualifier::Const
        }
    }

    /// `switch ...` in **statement** position. Desugars to an if/else-if chain
    /// of `HirStmt::If`; the subject (when present) is evaluated once into a
    /// hidden slot (stored via a pending statement). Arm bodies are ordinary
    /// statement blocks — no value is produced.
    pub(crate) fn analyze_switch_stmt(&mut self, switch: &ast::SwitchStmt) -> Option<HirStmt> {
        let subject_slot = match &switch.subject {
            Some(subject) => match self.eval_switch_subject(subject) {
                Some(s) => Some(s),
                None => {
                    // Still analyze the arms so their errors surface.
                    self.analyze_switch_arms_for_errors(switch);
                    return None;
                }
            },
            None => None,
        };

        // Analyze each conditional arm's condition + body, and set aside the
        // final default arm's body for the tail else. Malformed arm order is
        // diagnosed by `switch_arm_condition`; its blocks are still analyzed
        // in source order to surface their inner errors.
        let mut conds: Vec<HirExpr> = Vec::new();
        let mut bodies: Vec<Vec<HirStmt>> = Vec::new();
        let mut tail_else: Vec<HirStmt> = Vec::new();
        let mut seen_default = false;
        let mut ok = true;
        let mut control_qualifier = Self::initial_switch_control_qualifier(
            switch,
            subject_slot.as_ref().map(|(_, _, qualifier)| *qualifier),
        );
        let mut request_control = false;
        for arm in &switch.arms {
            // An arm condition is tested only when the earlier arms fail; its
            // pending statements run before the whole switch.
            let cond = self.without_arg_order_captures(|this| {
                this.switch_arm_condition(subject_slot.clone(), arm, &mut seen_default)
            });
            if let Some(condition) = &cond {
                control_qualifier = control_qualifier.join(condition.qualifier);
                request_control |= self.unresolved_request_control(condition);
            }
            let body = self.analyze_block(&arm.body, control_qualifier, request_control);
            match cond {
                Some(cond) => {
                    conds.push(cond);
                    bodies.push(body);
                }
                // `None` from a labelless arm is the `default`; from a labeled
                // arm it is a condition that failed to type.
                None if arm.label.is_none() => {
                    if tail_else.is_empty() {
                        tail_else = body;
                    }
                }
                None => ok = false,
            }
        }
        if !ok {
            return None;
        }

        Some(Self::fold_switch_chain(
            conds,
            bodies,
            tail_else,
            switch.span,
        ))
    }

    /// Analyze switch arms purely to surface their inner diagnostics when the
    /// subject already failed (cascade-safe: the results are discarded).
    pub(crate) fn analyze_switch_arms_for_errors(&mut self, switch: &ast::SwitchStmt) {
        let mut seen_default = false;
        for arm in &switch.arms {
            let _ = self.switch_arm_condition(None, arm, &mut seen_default);
            let _ = self.analyze_block(&arm.body, Qualifier::Const, false);
        }
    }

    /// Fold conditional arm conditions and bodies into a nested if/else-if
    /// chain (`HirStmt::If`), with `tail_else` as the final else (the default
    /// arm's body, or empty). Arms are tested in order. Always returns a
    /// single statement; an arm-less switch becomes an empty always-false if.
    pub(crate) fn fold_switch_chain(
        conds: Vec<HirExpr>,
        bodies: Vec<Vec<HirStmt>>,
        tail_else: Vec<HirStmt>,
        span: Span,
    ) -> HirStmt {
        // Build from the last conditional arm inward: each arm's else is the
        // chain built so far (initially the default/tail else).
        let mut else_branch = tail_else;
        let mut pairs: Vec<(HirExpr, Vec<HirStmt>)> = conds.into_iter().zip(bodies).collect();
        // The innermost (last) arm first.
        while let Some((cond, then_branch)) = pairs.pop() {
            let node = HirStmt::If {
                cond,
                then_branch,
                else_branch: std::mem::take(&mut else_branch),
                span,
            };
            else_branch = vec![node];
        }
        match else_branch.pop() {
            // The chain reduced to a single top-level `if`.
            Some(node) if else_branch.is_empty() => node,
            // No conditional arms: only the default (or nothing) ran. Wrap in
            // a `true`-guarded if so the body executes every bar as a single
            // statement.
            other => {
                let mut body = else_branch;
                if let Some(n) = other {
                    body.push(n);
                }
                HirStmt::If {
                    cond: HirExpr {
                        kind: HirExprKind::ConstBool(true),
                        ty: Type::Bool,
                        qualifier: Qualifier::Const,
                        span,
                    },
                    then_branch: body,
                    else_branch: Vec::new(),
                    span,
                }
            }
        }
    }

    pub(crate) fn analyze_block(
        &mut self,
        stmts: &[ast::Stmt],
        control_qualifier: Qualifier,
        request_control: bool,
    ) -> Vec<HirStmt> {
        self.scopes.push(HashMap::new());
        self.assignment_controls.push(control_qualifier);
        if request_control {
            self.request_branch_depth += 1;
        }
        let series_control = control_qualifier == Qualifier::Series;
        if series_control {
            self.branch_depth += 1;
        }
        let mut result = Vec::new();
        for (index, stmt) in stmts.iter().enumerate() {
            self.block_tail = index + 1 == stmts.len();
            self.analyze_stmt_into(stmt, &mut result);
        }
        self.block_tail = false;
        if series_control {
            self.branch_depth -= 1;
        }
        if request_control {
            self.request_branch_depth -= 1;
        }
        self.assignment_controls.pop();
        self.scopes.pop();
        result
    }

    /// `for counter = start to end [by step]`. The start/end/step expressions
    /// are evaluated **once** at loop entry in PineWASM; the counter is a
    /// loop-local variable in a fresh scope. The start and step determine the
    /// counter's type; a float end bound does not widen an int counter.
    /// A nonpositive numeric step compiles and fails when the loop executes.
    /// Stateful `ta.*` calls and compound-history materialization inside the
    /// body are `E0222` (strict divergence: per-call-site state assumes
    /// once-per-bar execution).
    pub(crate) fn analyze_for(&mut self, for_stmt: &ast::ForStmt) -> Option<HirStmt> {
        self.analyze_for_inner(for_stmt, false)
            .map(|(stmt, _)| stmt)
    }

    /// When a counted loop is the tail of a value block, retain its final
    /// expression from the latest iteration in a hidden slot. A loop that
    /// never evaluates that expression leaves the slot at `na`.
    pub(crate) fn analyze_for_inner(
        &mut self,
        for_stmt: &ast::ForStmt,
        return_value: bool,
    ) -> Option<(HirStmt, Option<HirExpr>)> {
        // Bounds are ordinary expressions evaluated at loop entry, in the
        // enclosing scope (the counter is not yet in scope).
        // The loop bounds are re-evaluated as the loop runs, while their
        // pending statements run once before it: keep them inline.
        let (start, end, step) = self.without_arg_order_captures(|this| {
            let start = this.analyze_expr(&for_stmt.start);
            let end = this.analyze_expr(&for_stmt.end);
            let step = for_stmt.step.as_ref().map(|s| this.analyze_expr(s));
            (start, end, step)
        });

        // Each bound must be numeric (int or float).
        let mut ok = true;
        for (bound, label) in [(&start, "start"), (&end, "end")] {
            if !bound.ty.is_error() && !is_numeric(&bound.ty) {
                self.error(
                    codes::E0222,
                    format!(
                        "`for` {label} bound must be int or float, found {}",
                        bound.ty.describe()
                    ),
                    bound.span,
                    "loop bounds are numeric",
                );
                ok = false;
            }
        }
        if let Some(step) = &step {
            if !step.ty.is_error() && !is_numeric(&step.ty) {
                self.error(
                    codes::E0222,
                    format!(
                        "`for` step must be int or float, found {}",
                        step.ty.describe()
                    ),
                    step.span,
                    "the loop step is numeric",
                );
                ok = false;
            }
        }

        // An int start and implicit/int step give an int counter, even when
        // the inclusive end bound is float. The comparison still uses the
        // original fractional end (e.g. `1 to 0.5` runs once).
        let is_int = start.ty == Type::Int && step.as_ref().is_none_or(|s| s.ty == Type::Int) && ok;
        let counter_ty = if is_int { Type::Int } else { Type::Float };
        // A counter may shadow a built-in variable like any declaration, but
        // not a built-in constant.
        self.note_builtin_shadowing(&for_stmt.counter);
        if self
            .registry
            .lookup_constant(&for_stmt.counter.name)
            .is_some()
        {
            self.error(
                codes::E0211,
                format!("cannot redeclare built-in `{}`", for_stmt.counter.name),
                for_stmt.counter.span,
                "built-in values cannot be used as loop counters",
            );
            ok = false;
        }

        // A float counter widens integer bounds. An int counter keeps a float
        // end bound for its termination comparisons.
        let (start, end, step) = if is_int {
            (start, end, step)
        } else {
            (
                Self::coerce_int_to_float(start),
                Self::coerce_int_to_float(end),
                step.map(Self::coerce_int_to_float),
            )
        };

        // Hidden non-persistent slots hold `step` and the current `end` value.
        // `end` is re-evaluated before each iteration; the initial value is
        // used to choose the direction when `by` is omitted.
        let end_slot = self.allocate_synthetic_slot(end.ty.clone(), end.span);
        let step_span = step.as_ref().map_or(for_stmt.span, |s| s.span);
        let step_slot = self.allocate_synthetic_slot(counter_ty.clone(), step_span);

        // The synthetic step value when no `by` clause is written; the actual
        // +1/-1 selection happens at MIR entry from the captured bounds.
        let implicit_step = step.is_none();
        let step = step.unwrap_or_else(|| HirExpr {
            kind: if is_int {
                HirExprKind::ConstInt(1)
            } else {
                HirExprKind::ConstFloat(1.0)
            },
            ty: counter_ty.clone(),
            qualifier: Qualifier::Const,
            span: for_stmt.span,
        });

        // Declare the counter in a fresh scope. It is a loop-local `series`
        // variable (it advances within the bar); assigning to it via `:=` is
        // `E0217`.
        self.scopes.push(HashMap::new());
        let counter_id = SymbolId(self.symbols.len() as u32);
        let counter_slot = SeriesSlot(counter_id.0);
        self.symbols.push(Symbol {
            id: counter_id,
            name: for_stmt.counter.name.clone(),
            ty: counter_ty,
            qualifier: Qualifier::Series,
            slot: counter_slot,
            persistent: false,
            varip: false,
            synthetic: false,
            const_int: None,
            decl_span: for_stmt.counter.span,
        });
        if for_stmt.counter.name != DISCARD_NAME {
            self.scopes
                .last_mut()
                .unwrap()
                .insert(for_stmt.counter.name.clone(), counter_id);
        }
        self.loop_counters.push(counter_id);

        self.loop_depth += 1;
        self.branch_depth += 1;
        // The body's last expression is the loop's value whether or not
        // anything reads it, so it is a block tail in statement position too.
        let mut body = self.analyze_block_no_scope(&for_stmt.body);
        self.branch_depth -= 1;
        self.loop_depth -= 1;
        self.scopes.pop();

        let result = if return_value {
            let tail = match body.last() {
                Some(HirStmt::Eval { expr, .. }) => Some(expr.clone()),
                Some(HirStmt::Assign { symbol, span, .. }) => {
                    let assigned = &self.symbols[symbol.0 as usize];
                    Some(HirExpr {
                        kind: HirExprKind::ReadVar {
                            symbol: *symbol,
                            offset: 0,
                        },
                        ty: assigned.ty.clone(),
                        qualifier: assigned.qualifier,
                        span: *span,
                    })
                }
                _ => None,
            };
            tail.filter(|expr| !expr.ty.is_error() && expr.ty != Type::Void)
                .map(|expr| {
                    if matches!(body.last(), Some(HirStmt::Eval { .. })) {
                        body.pop();
                    }
                    let slot = self.allocate_synthetic_slot(expr.ty.clone(), expr.span);
                    let symbol = SymbolId(slot.0);
                    let span = expr.span;
                    body.push(HirStmt::Assign {
                        symbol,
                        value: expr.clone(),
                        init: false,
                        span,
                    });
                    HirExpr {
                        kind: HirExprKind::ReadVar { symbol, offset: 0 },
                        ty: expr.ty,
                        qualifier: Qualifier::Series,
                        span,
                    }
                })
        } else {
            None
        };

        if !ok {
            return None;
        }
        Some((
            HirStmt::For {
                counter: counter_id,
                start,
                end,
                step,
                end_slot,
                step_slot,
                is_int,
                implicit_step,
                body,
                span: for_stmt.span,
            },
            result,
        ))
    }

    /// `for … in` iteration:
    /// - `for element in arr` / `for [index, element] in arr` over an
    ///   `array<T>`: each element in order;
    /// - `for row in m` / `for [index, row] in m` over a `matrix<T>`: each row
    ///   in order, as a new `array<T>` (`matrix.row`; one refilled hidden
    ///   row when the row cannot outlive its iteration);
    /// - `for [key, value] in m` over a `map<K, V>`: each entry in insertion
    ///   order. A map has no single-binding form.
    ///
    /// Desugars to a counted loop over the iterable's length that binds the
    /// element (and optional index or key) per iteration. The iterable is
    /// evaluated **once** into a hidden temp (so a side-effecting iterable
    /// expression runs once). The loop end is the counted loop's, which is
    /// re-evaluated before each iteration, so an array or matrix the body
    /// resizes is walked to its updated size.
    /// A map's entries are snapshotted at entry with `map.keys` and, when
    /// the body may write a map, `map.values`; a body that cannot reads each
    /// value with `map.get`, which then equals the snapshot. Changes to a
    /// map's entries during the loop are checked as follows: each iteration
    /// first compares `map.size` with the snapshot and stops with a runtime
    /// error when they differ. A change in the last iteration, or a remove
    /// and re-put that keeps the size, is not detected.
    /// Any other iterable is `E0235`.
    pub(crate) fn analyze_for_in(&mut self, for_in: &ast::ForInStmt) -> Option<HirStmt> {
        let span = for_in.span;
        // Analyze the iterable to check it is a collection. (This
        // borrow-checks the element type used for the binding.)
        let iterable = self.analyze_expr(&for_in.iterable);
        if iterable.ty.is_error() {
            // A broken iterable already produced a diagnostic; still analyze
            // the body in a fresh scope so its errors surface.
            return None;
        }
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Iterable {
            Array,
            Matrix,
            Map,
        }
        let kind = if iterable.ty.array_elem().is_some() {
            Iterable::Array
        } else if iterable.ty.matrix_elem().is_some() {
            Iterable::Matrix
        } else if iterable.ty.map_types().is_some() {
            Iterable::Map
        } else {
            self.error(
                codes::E0235,
                format!(
                    "`for … in` requires an array, a matrix, or a map, found {}",
                    iterable.ty.describe()
                ),
                for_in.iterable.span(),
                "the iterable of a `for … in` loop must be an array, a matrix, or a map",
            );
            return None;
        };
        if kind == Iterable::Map && for_in.index.is_none() {
            self.error(
                codes::E0235,
                format!(
                    "`for … in` over a map needs `[key, value]`, found the single binding `{}`",
                    for_in.element.name
                ),
                for_in.element.span,
                "write `for [key, value] in m`",
            );
            return None;
        }

        // Synthetic names use a `%` prefix so they can never collide with (or
        // be referenced by) a user identifier. The counter with a `%` name is
        // still a normal loop-local variable in `analyze_for`.
        let uid = self.forin_counter;
        self.forin_counter += 1;
        let iter_name = format!("%forin_iter{uid}");
        let counter_name = format!("%forin_i{uid}");

        // 1) `%forin_iterN = <iterable>` — bind the iterable once. Pushed to
        //    `pending_stmts` so it runs before the loop. The binding takes
        //    the iterable's HIR analyzed (and type-checked) above instead of
        //    analyzing the source again: a second analysis expanded a user
        //    function or method in the iterable a second time, so its body
        //    (any `array.push`, drawing or other effect) ran twice. The
        //    statements the first analysis hoisted are already pending, ahead
        //    of this assignment, as for any captured value.
        let iter_slot = self.allocate_synthetic_slot(iterable.ty.clone(), span);
        let iter_symbol = SymbolId(iter_slot.0);
        self.symbols[iter_symbol.0 as usize]
            .name
            .clone_from(&iter_name);
        self.symbols[iter_symbol.0 as usize].qualifier = iterable.qualifier;
        self.scopes
            .last_mut()
            .expect("a statement is analyzed inside a scope")
            .insert(iter_name.clone(), iter_symbol);
        self.pending_stmts.push(HirStmt::Assign {
            symbol: iter_symbol,
            value: iterable.clone(),
            init: true,
            span,
        });

        // Helpers to build synthetic AST fragments at `span`.
        let ident = |name: &str| {
            ast::Expr::Ident(ast::Ident {
                name: name.to_string(),
                span,
            })
        };
        let int_lit = |value: i64| ast::Expr::Int { value, span };
        let call = |callee: &str, args: Vec<ast::Expr>| ast::Expr::Call {
            callee: ast::Ident {
                name: callee.to_string(),
                span,
            },
            args: args
                .into_iter()
                .map(|value| ast::CallArg {
                    name: None,
                    value,
                    span,
                })
                .collect(),
            type_args: Vec::new(),
            span,
        };

        // A map iterates a snapshot of its entries taken before the loop:
        // `%forin_keysN = map.keys(%forin_iterN)` and, when the body may
        // write a map, `%forin_valuesN = map.values(%forin_iterN)`, which
        // list the entries in the same insertion order. Each iteration binds
        // the value its entry had at loop entry.
        //
        // A body that cannot write any map cannot change a value, so the
        // loop then reads each value with `map.get` and skips the values
        // snapshot: that copy held every entry a second time against
        // `RunOptions::max_collection_elements`, so the loop needed three
        // times the map's size and stopped a valid loop over a map larger
        // than a third of the budget. Without it a map of up to half the
        // budget (the 50,000-pair map limit, with the default budget of 100,000)
        // fits.
        let keys_name = format!("%forin_keys{uid}");
        let values_name = format!("%forin_values{uid}");
        let key_name = format!("%forin_key{uid}");
        let snapshot_values =
            kind == Iterable::Map && self.forin_body_may_write_a_map(&for_in.body);
        if kind == Iterable::Map {
            let mut snapshots = vec![(&keys_name, "map.keys")];
            if snapshot_values {
                snapshots.push((&values_name, "map.values"));
            }
            for (name, callee) in snapshots {
                let snapshot = ast::Stmt::Assign {
                    name: ast::Ident {
                        name: name.clone(),
                        span,
                    },
                    value: call(callee, vec![ident(&iter_name)]),
                    is_var: false,
                    is_varip: false,
                    annotation: None,
                    span,
                };
                if let Some(s) = self.analyze_stmt(&snapshot) {
                    self.pending_stmts.push(s);
                }
            }
        }
        // A matrix row that the body only reads or changes in place, through
        // builtin `array.*` calls and methods that neither return, store nor
        // view it, cannot be seen after its iteration. Such a loop refills one hidden row array per iteration
        // (`array.clear`, then `array.push` of each `matrix.get` cell) instead
        // of allocating a new `matrix.row` each time. A new row stays charged
        // to `RunOptions::max_collection_elements` until the next bar's
        // collection, so walking every row of a matrix larger than half the
        // budget used to stop a valid loop with `CollectionLimitExceeded`.
        // A row that may outlive its iteration keeps `matrix.row`.
        let row_buffer = format!("%forin_row{uid}");
        let reuse_row = kind == Iterable::Matrix
            && iterable.ty.matrix_elem().is_some_and(|elem| {
                Self::forin_row_buffer_ctor(elem, span).is_some()
                    && self.forin_row_is_iteration_local(
                        &for_in.element.name,
                        &Type::Array(Box::new(elem.clone())),
                        &for_in.body,
                    )
            });
        if reuse_row {
            let elem = iterable.ty.matrix_elem().expect("checked above");
            let buffer = ast::Stmt::Assign {
                name: ast::Ident {
                    name: row_buffer.clone(),
                    span,
                },
                value: Self::forin_row_buffer_ctor(elem, span).expect("checked above"),
                is_var: false,
                is_varip: false,
                annotation: None,
                span,
            };
            if let Some(s) = self.analyze_stmt(&buffer) {
                self.pending_stmts.push(s);
            }
        }
        let length = || match kind {
            Iterable::Array => call("array.size", vec![ident(&iter_name)]),
            Iterable::Matrix => call("matrix.rows", vec![ident(&iter_name)]),
            Iterable::Map => call("array.size", vec![ident(&keys_name)]),
        };
        let (element, index) = match kind {
            Iterable::Array => (
                call("array.get", vec![ident(&iter_name), ident(&counter_name)]),
                ident(&counter_name),
            ),
            Iterable::Matrix if reuse_row => (ident(&row_buffer), ident(&counter_name)),
            Iterable::Matrix => (
                call("matrix.row", vec![ident(&iter_name), ident(&counter_name)]),
                ident(&counter_name),
            ),
            Iterable::Map if snapshot_values => (
                call("array.get", vec![ident(&values_name), ident(&counter_name)]),
                ident(&key_name),
            ),
            Iterable::Map => (
                call("map.get", vec![ident(&iter_name), ident(&key_name)]),
                ident(&key_name),
            ),
        };

        // 2) Build the counted loop body:
        //      element = array.get(%forin_iter, %forin_i)   (array)
        //              | matrix.row(%forin_iter, %forin_i)  (matrix)
        //              | array.get(%forin_values, %forin_i) (map)
        //      [index  = %forin_i                           (array, matrix)
        //              | array.get(%forin_keys, %forin_i)]  (map; tuple form)
        //      <original body>
        let mut body: Vec<ast::Stmt> = Vec::with_capacity(for_in.body.len() + 5);
        if reuse_row {
            // array.clear(%forin_rowN)
            // if matrix.columns(%forin_iterN) > 0
            //     for %forin_cN = 0 to matrix.columns(%forin_iterN) - 1 by 1
            //         array.push(%forin_rowN, matrix.get(%forin_iterN, %forin_iN, %forin_cN))
            let column = format!("%forin_c{uid}");
            let columns = || call("matrix.columns", vec![ident(&iter_name)]);
            body.push(ast::Stmt::Expr {
                expr: call("array.clear", vec![ident(&row_buffer)]),
                span,
            });
            let fill = ast::ForStmt {
                counter: ast::Ident {
                    name: column.clone(),
                    span,
                },
                start: int_lit(0),
                end: ast::Expr::Binary {
                    op: ast::BinaryOp::Sub,
                    lhs: Box::new(columns()),
                    rhs: Box::new(int_lit(1)),
                    span,
                },
                step: Some(int_lit(1)),
                body: vec![ast::Stmt::Expr {
                    expr: call(
                        "array.push",
                        vec![
                            ident(&row_buffer),
                            call(
                                "matrix.get",
                                vec![ident(&iter_name), ident(&counter_name), ident(&column)],
                            ),
                        ],
                    ),
                    span,
                }],
                span,
            };
            body.push(ast::Stmt::If(ast::IfStmt {
                cond: ast::Expr::Binary {
                    op: ast::BinaryOp::Gt,
                    lhs: Box::new(columns()),
                    rhs: Box::new(int_lit(0)),
                    span,
                },
                then_branch: vec![ast::Stmt::For(Box::new(fill))],
                else_branch: None,
                span,
            }));
        }
        if kind == Iterable::Map {
            // Adding or removing a map's entries while a
            // `for [key, value] in` loop walks it is a runtime error. The
            // entry count is checked before each iteration; the first always
            // matches the snapshot.
            body.push(ast::Stmt::If(ast::IfStmt {
                cond: ast::Expr::Binary {
                    op: ast::BinaryOp::Ne,
                    lhs: Box::new(call("map.size", vec![ident(&iter_name)])),
                    rhs: Box::new(call("array.size", vec![ident(&keys_name)])),
                    span,
                },
                then_branch: vec![ast::Stmt::Expr {
                    expr: call(
                        "runtime.error",
                        vec![ast::Expr::Str {
                            value: "Cannot add or remove map entries while a `for [key, value] in` loop iterates over the map. Loop over a copy (`map.copy()`) or over `map.keys()` instead.".to_string(),
                            span,
                        }],
                    ),
                    span,
                }],
                else_branch: None,
                span,
            }));
        }
        if kind == Iterable::Map {
            // `%forin_keyN = array.get(%forin_keysN, %forin_iN)`, read once
            // for both the key binding and the value's `map.get`.
            body.push(ast::Stmt::Assign {
                name: ast::Ident {
                    name: key_name.clone(),
                    span,
                },
                value: call("array.get", vec![ident(&keys_name), ident(&counter_name)]),
                is_var: false,
                is_varip: false,
                annotation: None,
                span,
            });
        }
        body.push(ast::Stmt::Assign {
            name: for_in.element.clone(),
            value: element,
            is_var: false,
            is_varip: false,
            annotation: None,
            span: for_in.element.span,
        });
        if let Some(index_name) = &for_in.index {
            body.push(ast::Stmt::Assign {
                name: index_name.clone(),
                value: index,
                is_var: false,
                is_varip: false,
                annotation: None,
                span: index_name.span,
            });
        }
        body.extend(for_in.body.iter().cloned());

        // 3) Guard the counted loop with `length > 0`. A positive `by 1`
        //    follows the initial bounds' direction, so an unguarded empty
        //    collection (`end = -1`) would descend into a spurious iteration.
        let end = ast::Expr::Binary {
            op: ast::BinaryOp::Sub,
            lhs: Box::new(length()),
            rhs: Box::new(int_lit(1)),
            span,
        };
        let counted = ast::ForStmt {
            counter: ast::Ident {
                name: counter_name,
                span,
            },
            start: int_lit(0),
            end,
            step: Some(int_lit(1)),
            body,
            span,
        };
        let guard = ast::IfStmt {
            cond: ast::Expr::Binary {
                op: ast::BinaryOp::Gt,
                lhs: Box::new(length()),
                rhs: Box::new(int_lit(0)),
                span,
            },
            then_branch: vec![ast::Stmt::For(Box::new(counted))],
            else_branch: None,
            span,
        };
        self.analyze_if(&guard)
    }

    /// Whether `body` may write the entries of any map: a `map.put`,
    /// `map.remove`, `map.clear` or `map.put_all` call (namespace or method
    /// form, on any receiver, since another variable may name the iterated
    /// map), or a call this analysis cannot see into (a user function or
    /// method, an unregistered namespace, a chained method call). A `for
    /// [key, value] in m` body for which this is false cannot change a
    /// value, so the loop reads values with `map.get` instead of snapshotting
    /// them.
    fn forin_body_may_write_a_map(&self, body: &[ast::Stmt]) -> bool {
        let mut writes = false;
        for stmt in body {
            forin_row_visit_stmt(stmt, &mut |expr: &ast::Expr| {
                writes |= self.forin_expr_may_write_a_map(expr);
            });
        }
        writes
    }

    fn forin_expr_may_write_a_map(&self, expr: &ast::Expr) -> bool {
        const MAP_WRITERS: [&str; 4] = ["put", "remove", "clear", "put_all"];
        let any = |exprs: &[&ast::Expr]| exprs.iter().any(|e| self.forin_expr_may_write_a_map(e));
        let block = |stmts: &[ast::Stmt]| self.forin_body_may_write_a_map(stmts);
        match expr {
            ast::Expr::Int { .. }
            | ast::Expr::Float { .. }
            | ast::Expr::Bool { .. }
            | ast::Expr::Na { .. }
            | ast::Expr::Str { .. }
            | ast::Expr::Color { .. }
            | ast::Expr::Ident(_) => false,
            ast::Expr::Unary { operand, .. } => self.forin_expr_may_write_a_map(operand),
            ast::Expr::Binary { lhs, rhs, .. } => any(&[lhs.as_ref(), rhs.as_ref()]),
            ast::Expr::Ternary {
                cond,
                then_e,
                else_e,
                ..
            } => any(&[cond.as_ref(), then_e.as_ref(), else_e.as_ref()]),
            ast::Expr::Call { callee, args, .. } => {
                let name = callee.name.as_str();
                let opaque_or_writer = if let Some(func) = self.registry.lookup_function(name) {
                    name.strip_prefix("map.")
                        .is_some_and(|member| MAP_WRITERS.contains(&member))
                        || !matches!(
                            func.kind,
                            BuiltinFunctionKind::Collection(_)
                                | BuiltinFunctionKind::Pure(_)
                                | BuiltinFunctionKind::Ta(_)
                                | BuiltinFunctionKind::Plot
                                | BuiltinFunctionKind::Display(_)
                                | BuiltinFunctionKind::Log(_)
                                | BuiltinFunctionKind::Alert
                                | BuiltinFunctionKind::MapConstructor { .. }
                                | BuiltinFunctionKind::MatrixConstructor { .. }
                        )
                } else {
                    match name.rsplit_once('.') {
                        Some((prefix, method)) if !prefix.contains('.') => {
                            if let Some(symbol) = self.lookup_variable(prefix) {
                                let ty = &self.symbols[symbol.0 as usize].ty;
                                MAP_WRITERS.contains(&method)
                                    || self
                                        .find_method(&(
                                            Self::value_type_key(ty),
                                            method.to_string(),
                                        ))
                                        .is_some()
                            } else {
                                // `Type.new` / `Type.copy` build or copy an
                                // object; they write no map's entries.
                                !(self.object_type_name(prefix).is_some()
                                    && matches!(method, "new" | "copy"))
                            }
                        }
                        // A user function, or a namespace this check does not
                        // know.
                        _ => true,
                    }
                };
                opaque_or_writer || any(&args.iter().map(|arg| &arg.value).collect::<Vec<_>>())
            }
            // A chained method call's receiver type is not known here.
            ast::Expr::MethodCall { .. } => true,
            ast::Expr::FieldAccess { receiver, .. } => self.forin_expr_may_write_a_map(receiver),
            ast::Expr::History { target, offset, .. } => any(&[target.as_ref(), offset.as_ref()]),
            ast::Expr::Group { inner, .. } => self.forin_expr_may_write_a_map(inner),
            ast::Expr::If(if_expr) => {
                self.forin_expr_may_write_a_map(&if_expr.cond)
                    || block(&if_expr.then_branch)
                    || if_expr.else_branch.as_deref().is_some_and(block)
            }
            ast::Expr::Switch(switch) => {
                switch
                    .subject
                    .as_ref()
                    .is_some_and(|s| self.forin_expr_may_write_a_map(s))
                    || switch.arms.iter().any(|arm| {
                        arm.label
                            .as_ref()
                            .is_some_and(|l| self.forin_expr_may_write_a_map(l))
                            || block(&arm.body)
                    })
            }
            ast::Expr::Loop(stmt) => block(std::slice::from_ref(stmt.as_ref())),
            ast::Expr::Tuple { elements, .. } => any(&elements.iter().collect::<Vec<_>>()),
        }
    }

    /// An empty `array<elem>` for the reused `for row in m` buffer, or `None`
    /// for an element type with no constructor here (the loop then keeps
    /// `matrix.row`).
    fn forin_row_buffer_ctor(elem: &Type, span: Span) -> Option<ast::Expr> {
        let (callee, type_args) = match elem {
            Type::Float => ("array.new_float", Vec::new()),
            Type::Int => ("array.new_int", Vec::new()),
            Type::Bool => ("array.new_bool", Vec::new()),
            Type::String => ("array.new_string", Vec::new()),
            Type::Color => ("array.new_color", Vec::new()),
            Type::Enum(name) | Type::Object(name) if !name.contains('@') => (
                "array.new",
                vec![ast::Ident {
                    name: name.clone(),
                    span,
                }],
            ),
            _ => return None,
        };
        Some(ast::Expr::Call {
            callee: ast::Ident {
                name: callee.to_string(),
                span,
            },
            args: vec![ast::CallArg {
                name: None,
                value: ast::Expr::Int { value: 0, span },
                span,
            }],
            type_args,
            span,
        })
    }

    /// Whether every use of the `for row in m` binding `row` in `body` is
    /// the receiver of a builtin array call that cannot let it escape
    /// (`array.get(row, i)`, `row.sum()`, `row.push(x)`; see
    /// `FORIN_ROW_LOCAL_ARRAY_MEMBERS`), so no reference to the row can
    /// outlive its iteration. Anything else, including passing `row` to a
    /// user function or method, storing it, reading its history, returning
    /// it as the loop's value, or a builtin that returns, stores or views it
    /// (`array.concat`, `array.slice`, `array.from`, `matrix.add_row`), may
    /// keep it, and the loop then allocates each row.
    fn forin_row_is_iteration_local(&self, row: &str, row_ty: &Type, body: &[ast::Stmt]) -> bool {
        let row_method = |method: &str| {
            FORIN_ROW_LOCAL_ARRAY_MEMBERS.contains(&method)
                && self
                    .find_method(&(Self::value_type_key(row_ty), method.to_string()))
                    .is_none()
        };
        let mut local = true;
        for stmt in body {
            forin_row_visit_stmt(stmt, &mut |expr: &ast::Expr| {
                local &= forin_row_expr_is_local(row, &row_method, expr);
            });
        }
        local
    }

    /// `while cond` — the condition must be bool (`E0208`), re-checked every
    /// iteration. Stateful `ta.*` calls / compound history in the body are
    /// `E0222`, exactly as in `for`.
    pub(crate) fn analyze_while(&mut self, while_stmt: &ast::WhileStmt) -> Option<HirStmt> {
        // The condition is tested before every iteration; its pending
        // statements run once before the loop. Keep it inline.
        let cond = self.without_arg_order_captures(|this| this.analyze_expr(&while_stmt.cond));
        if !cond.ty.is_error() && cond.ty != Type::Bool {
            self.error(
                codes::E0208,
                format!(
                    "`while` condition must be bool, found {}",
                    cond.ty.describe()
                ),
                while_stmt.cond.span(),
                "expected a boolean expression",
            );
        }
        self.scopes.push(HashMap::new());
        self.loop_depth += 1;
        self.branch_depth += 1;
        let body = self.analyze_block_no_scope(&while_stmt.body);
        self.branch_depth -= 1;
        self.loop_depth -= 1;
        self.scopes.pop();
        Some(HirStmt::While {
            cond,
            body,
            span: while_stmt.span,
        })
    }

    /// A loop body, analyzed in the caller's already-pushed scope (the loop
    /// pushes the scope so the counter lives in it).
    ///
    /// A loop is an expression whose value is the last expression of its
    /// body, exactly as for an `if` block ([`Self::analyze_block`]). That
    /// statement is the block's value, not a discarded one.
    pub(crate) fn analyze_block_no_scope(&mut self, stmts: &[ast::Stmt]) -> Vec<HirStmt> {
        let mut result = Vec::new();
        for (index, stmt) in stmts.iter().enumerate() {
            self.block_tail = index + 1 == stmts.len();
            self.analyze_stmt_into(stmt, &mut result);
        }
        self.block_tail = false;
        result
    }

    /// `break` / `continue` — valid only inside a loop body (`E0223`
    /// otherwise). Affects the innermost loop.
    pub(crate) fn analyze_break_continue(&mut self, is_break: bool, span: Span) -> Option<HirStmt> {
        if self.loop_depth == 0 {
            let kw = if is_break { "break" } else { "continue" };
            self.error(
                codes::E0223,
                format!("`{kw}` outside of a loop"),
                span,
                format!("`{kw}` is only valid inside a `for` or `while` body"),
            );
            return None;
        }
        Some(if is_break {
            HirStmt::Break { span }
        } else {
            HirStmt::Continue { span }
        })
    }
}

/// Call `check` on every top-level expression of `stmt` and of the
/// statements nested in it (see `Analyzer::forin_row_is_iteration_local`).
fn forin_row_visit_stmt(stmt: &ast::Stmt, check: &mut dyn FnMut(&ast::Expr)) {
    let block = |stmts: &[ast::Stmt], check: &mut dyn FnMut(&ast::Expr)| {
        for stmt in stmts {
            forin_row_visit_stmt(stmt, check);
        }
    };
    match stmt {
        ast::Stmt::Assign { value, .. }
        | ast::Stmt::Reassign { value, .. }
        | ast::Stmt::Destructure { value, .. }
        | ast::Stmt::Expr { expr: value, .. } => check(value),
        ast::Stmt::If(if_stmt) => {
            check(&if_stmt.cond);
            block(&if_stmt.then_branch, check);
            if let Some(else_branch) = &if_stmt.else_branch {
                block(else_branch, check);
            }
        }
        ast::Stmt::Once(once) => {
            if let Some(cond) = &once.cond {
                check(cond);
            }
            block(&once.body, check);
        }
        ast::Stmt::For(for_stmt) => {
            check(&for_stmt.start);
            check(&for_stmt.end);
            if let Some(step) = &for_stmt.step {
                check(step);
            }
            block(&for_stmt.body, check);
        }
        ast::Stmt::ForIn(for_in) => {
            check(&for_in.iterable);
            block(&for_in.body, check);
        }
        ast::Stmt::While(while_stmt) => {
            check(&while_stmt.cond);
            block(&while_stmt.body, check);
        }
        ast::Stmt::Switch(switch) => {
            if let Some(subject) = &switch.subject {
                check(subject);
            }
            for arm in &switch.arms {
                if let Some(label) = &arm.label {
                    check(label);
                }
                block(&arm.body, check);
            }
        }
        ast::Stmt::Break { .. } | ast::Stmt::Continue { .. } => {}
    }
}

/// The builtin `array.*` members that take a row as their receiver without
/// letting it outlive the call: each returns nothing, an element or other
/// scalar, or a new array (`copy`, `sort_indices`), and none stores its
/// receiver. Anything else, such as `concat` (which returns its receiver),
/// `slice` (a view of it) or a member not listed here, may alias the row, so
/// the loop then gives each row its own array.
const FORIN_ROW_LOCAL_ARRAY_MEMBERS: &[&str] = &[
    "avg",
    "binary_search",
    "binary_search_leftmost",
    "binary_search_rightmost",
    "clear",
    "copy",
    "covariance",
    "every",
    "fill",
    "first",
    "get",
    "includes",
    "indexof",
    "insert",
    "join",
    "last",
    "lastindexof",
    "max",
    "median",
    "min",
    "mode",
    "percentile_linear_interpolation",
    "percentile_nearest_rank",
    "percentrank",
    "pop",
    "push",
    "range",
    "remove",
    "reverse",
    "set",
    "shift",
    "size",
    "some",
    "sort",
    "sort_indices",
    "stdev",
    "sum",
    "unshift",
    "variance",
];

/// Whether `expr` uses `row` only as the receiver of a builtin array call
/// that cannot let it escape: the first argument of an `array.*` call whose
/// member is in [`FORIN_ROW_LOCAL_ARRAY_MEMBERS`], or the receiver of a
/// method `row_method` accepts.
fn forin_row_expr_is_local(row: &str, row_method: &dyn Fn(&str) -> bool, expr: &ast::Expr) -> bool {
    let local = |e: &ast::Expr| forin_row_expr_is_local(row, row_method, e);
    let is_row = |e: &ast::Expr| matches!(e, ast::Expr::Ident(id) if id.name == row);
    let block_local = |stmts: &[ast::Stmt]| {
        let mut ok = true;
        for stmt in stmts {
            forin_row_visit_stmt(stmt, &mut |e: &ast::Expr| ok &= local(e));
        }
        ok
    };
    match expr {
        ast::Expr::Int { .. }
        | ast::Expr::Float { .. }
        | ast::Expr::Bool { .. }
        | ast::Expr::Na { .. }
        | ast::Expr::Str { .. }
        | ast::Expr::Color { .. } => true,
        // A bare use, or a dotted name rooted at `row`, is not a receiver.
        ast::Expr::Ident(id) => {
            id.name != row
                && !id
                    .name
                    .strip_prefix(row)
                    .is_some_and(|rest| rest.starts_with('.'))
        }
        ast::Expr::Unary { operand, .. } => local(operand),
        ast::Expr::Binary { lhs, rhs, .. } => local(lhs) && local(rhs),
        ast::Expr::Ternary {
            cond,
            then_e,
            else_e,
            ..
        } => local(cond) && local(then_e) && local(else_e),
        ast::Expr::Call { callee, args, .. } => {
            let receiver_ok = match callee
                .name
                .strip_prefix(row)
                .and_then(|rest| rest.strip_prefix('.'))
            {
                Some(method) => !method.contains('.') && row_method(method),
                None => true,
            };
            let builtin_receiver = callee
                .name
                .strip_prefix("array.")
                .is_some_and(|member| FORIN_ROW_LOCAL_ARRAY_MEMBERS.contains(&member));
            receiver_ok
                && args.iter().enumerate().all(|(index, arg)| {
                    let receiver_slot =
                        index == 0 && arg.name.as_ref().is_none_or(|name| name.name == "id");
                    (builtin_receiver && receiver_slot && is_row(&arg.value)) || local(&arg.value)
                })
        }
        ast::Expr::MethodCall {
            receiver,
            method,
            args,
            ..
        } => {
            (if is_row(receiver) {
                row_method(&method.name)
            } else {
                local(receiver)
            }) && args.iter().all(|arg| local(&arg.value))
        }
        ast::Expr::FieldAccess { receiver, .. } => local(receiver),
        ast::Expr::History { target, offset, .. } => local(target) && local(offset),
        ast::Expr::Group { inner, .. } => local(inner),
        ast::Expr::If(if_expr) => {
            local(&if_expr.cond)
                && block_local(&if_expr.then_branch)
                && if_expr.else_branch.as_deref().is_none_or(block_local)
        }
        ast::Expr::Switch(switch) => {
            switch.subject.as_ref().is_none_or(local)
                && switch
                    .arms
                    .iter()
                    .all(|arm| arm.label.as_ref().is_none_or(local) && block_local(&arm.body))
        }
        ast::Expr::Loop(stmt) => block_local(std::slice::from_ref(stmt.as_ref())),
        ast::Expr::Tuple { elements, .. } => elements.iter().all(local),
    }
}
