//! Collection call analysis: `array.*`, `matrix.*`, and `map.*` constructors
//! and operations, element-type checking and coercion, and the shared
//! collection argument helpers.

use crate::*;

impl<'r> Analyzer<'r> {
    /// Check an `array.*` collection call. Collection functions have
    /// receiver-dependent result types, so this bespoke analyzer replaces the
    /// generic parameter framework. It enforces:
    /// - arity (`E0204`) against [`CollectionFunc::argc_bounds`] surface forms;
    /// - the receiver must be an `array<T>` (`E0235` on a non-collection
    ///   receiver);
    /// - element arguments must match the receiver's element type strictly
    ///   (`E0234`), with int → float widening the only allowed coercion (v6);
    /// - index/size arguments must be int; `array.join`'s separator string.
    ///
    /// It materializes constructor `(size, initial)` defaults so the emitted
    /// [`HirExprKind::CallCollection`] is fully explicit.
    pub(crate) fn analyze_collection_call(
        &mut self,
        func: CollectionFunc,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        // Collection names use the same documented parameter binding as scalar
        // built-ins. Preserve trailing omissions for each operation's existing
        // default handling. Variadic array.from has a separate surface.
        let normalized_args = if func != CollectionFunc::ArrayFrom
            && args.iter().any(|arg| arg.name.is_some())
        {
            let (param_names, _) = func.surface_params();
            let zero = ast::Expr::Int { value: 0, span };
            let minus_one = ast::Expr::Int { value: -1, span };
            let mut defaults = vec![None; param_names.len()];
            match func {
                CollectionFunc::ArrayNewFloat
                | CollectionFunc::ArrayNewInt
                | CollectionFunc::ArrayNewBool
                | CollectionFunc::ArrayNewString
                | CollectionFunc::ArrayNewColor => defaults[0] = Some(&zero),
                CollectionFunc::ArrayFill => defaults[2] = Some(&zero),
                CollectionFunc::MatrixSort => {
                    defaults[1] = Some(&zero);
                    defaults[2] = Some(&zero);
                }
                // A bound skipped by name takes the sentinel the operation
                // uses for a trailing omission: 0 for a start, -1 (to the
                // last row or column) for an end.
                CollectionFunc::MatrixFill => {
                    defaults[2] = Some(&zero);
                    defaults[3] = Some(&minus_one);
                    defaults[4] = Some(&zero);
                    defaults[5] = Some(&minus_one);
                }
                _ => {}
            }
            let Some(bound) =
                self.bind_positional_with_defaults(func.name(), param_names, &defaults, args, span)
            else {
                return self.error_expr(span);
            };
            Some(
                bound
                    .into_iter()
                    .map(|value| ast::CallArg {
                        span: value.span(),
                        name: None,
                        value,
                    })
                    .collect::<Vec<_>>(),
            )
        } else {
            for arg in args {
                if arg.name.is_some() {
                    self.error(
                        codes::E0235,
                        format!("`{}` does not take named arguments", func.name()),
                        arg.value.span(),
                        "pass arguments positionally",
                    );
                    for a in args {
                        let _ = self.analyze_expr(&a.value);
                    }
                    return self.error_expr(span);
                }
            }
            None
        };
        let args = normalized_args.as_deref().unwrap_or(args);

        match func {
            CollectionFunc::ArrayNewFloat
            | CollectionFunc::ArrayNewInt
            | CollectionFunc::ArrayNewBool
            | CollectionFunc::ArrayNewString
            | CollectionFunc::ArrayNewColor => self.analyze_array_constructor(func, args, span),
            // The object array constructor is built directly by
            // `analyze_object_collection_ctor` (it carries a type-tag arg and is
            // not a registered function), so it never dispatches through here.
            CollectionFunc::ArrayNewObject => {
                debug_assert!(false, "ArrayNewObject is constructed directly");
                self.error_expr(span)
            }
            CollectionFunc::ArrayFrom => self.analyze_array_from(func, args, span),
            CollectionFunc::StrSplit => self.analyze_str_split(func, args, span),
            // map.* receiver operations. `map.new_*` is a distinct
            // registry kind and never reaches here.
            CollectionFunc::MapPut
            | CollectionFunc::MapGet
            | CollectionFunc::MapContains
            | CollectionFunc::MapRemove
            | CollectionFunc::MapSize
            | CollectionFunc::MapClear
            | CollectionFunc::MapKeys
            | CollectionFunc::MapValues
            | CollectionFunc::MapCopy
            | CollectionFunc::MapPutAll => self.analyze_map_operation(func, args, span),
            CollectionFunc::MapNew => {
                // Unreachable: the constructor arrives via MapConstructor kind.
                self.error_expr(span)
            }
            // matrix.* receiver operations. `matrix.new_*` is a distinct
            // registry kind and never reaches here.
            CollectionFunc::MatrixGet
            | CollectionFunc::MatrixSet
            | CollectionFunc::MatrixRows
            | CollectionFunc::MatrixColumns
            | CollectionFunc::MatrixElementsCount
            | CollectionFunc::MatrixFill
            | CollectionFunc::MatrixCopy
            | CollectionFunc::MatrixAddRow
            | CollectionFunc::MatrixAddCol
            | CollectionFunc::MatrixRemoveRow
            | CollectionFunc::MatrixRemoveCol
            | CollectionFunc::MatrixSwapRows
            | CollectionFunc::MatrixSwapColumns
            | CollectionFunc::MatrixReverse
            | CollectionFunc::MatrixTranspose
            | CollectionFunc::MatrixReshape
            | CollectionFunc::MatrixSubmatrix
            | CollectionFunc::MatrixConcat
            | CollectionFunc::MatrixSum
            | CollectionFunc::MatrixDiff
            | CollectionFunc::MatrixMult
            | CollectionFunc::MatrixDet
            | CollectionFunc::MatrixInv
            | CollectionFunc::MatrixTrace
            | CollectionFunc::MatrixPow
            | CollectionFunc::MatrixKron
            | CollectionFunc::MatrixAvg
            | CollectionFunc::MatrixMin
            | CollectionFunc::MatrixMax
            | CollectionFunc::MatrixMedian
            | CollectionFunc::MatrixMode
            | CollectionFunc::MatrixIsSquare
            | CollectionFunc::MatrixIsIdentity
            | CollectionFunc::MatrixIsDiagonal
            | CollectionFunc::MatrixIsSymmetric
            | CollectionFunc::MatrixIsAntisymmetric
            | CollectionFunc::MatrixIsTriangular
            | CollectionFunc::MatrixIsStochastic
            | CollectionFunc::MatrixIsBinary
            | CollectionFunc::MatrixIsZero
            | CollectionFunc::MatrixIsAntidiagonal
            | CollectionFunc::MatrixRank
            | CollectionFunc::MatrixRow
            | CollectionFunc::MatrixCol
            | CollectionFunc::MatrixEigenvalues
            | CollectionFunc::MatrixEigenvectors
            | CollectionFunc::MatrixPinv
            | CollectionFunc::MatrixSort => self.analyze_matrix_operation(func, args, span),
            CollectionFunc::MatrixNew => {
                // Unreachable: arrives via MatrixConstructor kind.
                self.error_expr(span)
            }
            _ => self.analyze_array_operation(func, args, span),
        }
    }

    /// `map.new_<key>_<val>()` — a fresh empty `map<K, V>`. The key/value types
    /// come from the registry entry (the parser rewrites a scalar
    /// `map.new<K, V>` source form to that name); the emitted call carries them
    /// as two `ConstInt` type tags.
    pub(crate) fn analyze_map_constructor(
        &mut self,
        key: Type,
        val: Type,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        for arg in args {
            let _ = self.analyze_expr(&arg.value);
        }
        if !args.is_empty() {
            self.error(
                codes::E0204,
                format!(
                    "`map.new_{}_{}` expects 0 arguments, found {}",
                    key.describe(),
                    val.describe(),
                    args.len()
                ),
                span,
                "map constructors take no arguments",
            );
            return self.error_expr(span);
        }
        let key_tag =
            registry::scalar_type_tag(&key).expect("map key type is always a scalar value type");
        let val_tag =
            registry::scalar_type_tag(&val).expect("map value type is always a scalar value type");
        HirExpr {
            kind: HirExprKind::CallCollection {
                func: CollectionFunc::MapNew,
                args: vec![
                    Self::const_int_expr(key_tag, span),
                    Self::const_int_expr(val_tag, span),
                ],
            },
            ty: Type::Map(Box::new(key), Box::new(val)),
            // A fresh collection is a distinct mutable object each run.
            qualifier: Qualifier::Series,
            span,
        }
    }

    /// `matrix.new_<T>(rows = 0, columns = 0, initial_value = na)` — a fresh
    /// matrix. The parser rewrites generic `matrix.new<T>` source forms to
    /// this analyzer, whose element type drives the checks; the emitted call
    /// carries `(rows, columns, initial_value, elem_ty_tag)`.
    ///
    /// Arguments bind by position, then by the v6 parameter name, through the
    /// shared [`Self::bind_positional`] binder: an unknown or duplicate name is
    /// `E0228`, and a required parameter left unbound by naming a later one is
    /// `E0204`. The bound arguments are checked in parameter order, so a named
    /// call lowers to exactly the HIR of the equivalent positional call.
    pub(crate) fn analyze_matrix_constructor(
        &mut self,
        elem: Type,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let callee = format!("matrix.new_{}", elem.describe());
        let (param_names, _) = CollectionFunc::MatrixNew.surface_params();
        let first_binding_diagnostic = self.diagnostics.mark();
        let zero = ast::Expr::Int { value: 0, span };
        // A v6 `bool` is never `na`, so a `bool` matrix starts out `false`.
        let missing = if elem == Type::Bool {
            ast::Expr::Bool { value: false, span }
        } else {
            ast::Expr::Na { span }
        };
        let defaults = [Some(&zero), Some(&zero), Some(&missing)];
        let Some(mut bound) =
            self.bind_positional_with_defaults(&callee, param_names, &defaults, args, span)
        else {
            // The call is already rejected. Like the other binder callers,
            // skip argument analysis: a value no longer has an expected type
            // here, so a named `na` would only add an `E0220` cascade.
            self.note_renamed_matrix_initial(&callee, first_binding_diagnostic);
            return self.error_expr(span);
        };
        // Surface: matrix.new_<T>(rows = 0, columns = 0, initial_value = na).
        if bound.len() > 3 {
            self.error(
                codes::E0204,
                format!(
                    "`{callee}` expects 0 to 3 argument(s), found {}",
                    args.len()
                ),
                span,
                "matrix constructors take (rows, columns, initial_value), all optional",
            );
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        }
        while bound.len() < 3 {
            bound.push(if bound.len() < 2 {
                zero.clone()
            } else {
                missing.clone()
            });
        }
        // Analyze every bound argument before coercing any of them, so an
        // ill-typed leading argument does not hide errors in later ones. Each
        // bound expression keeps its own source span for diagnostics.
        let rows = self.analyze_expr_expecting(&bound[0], Some(&Type::Int));
        let cols = self.analyze_expr_expecting(&bound[1], Some(&Type::Int));
        let initial = bound.get(2).map(|value| {
            (
                self.analyze_expr_expecting(value, Some(&elem)),
                value.span(),
            )
        });
        let rows = self.coerce_matrix_dimension(param_names[0], rows, bound[0].span());
        let cols = self.coerce_matrix_dimension(param_names[1], cols, bound[1].span());
        // The initial value (default na when omitted).
        let initial = match initial {
            Some((value, value_span)) => {
                self.coerce_element_arg(CollectionFunc::MatrixNew, &elem, value, value_span)
            }
            None => Some(HirExpr {
                kind: HirExprKind::ConstNa,
                ty: elem.clone(),
                qualifier: Qualifier::Const,
                span,
            }),
        };
        let (Some(rows), Some(cols), Some(initial)) = (rows, cols, initial) else {
            return self.error_expr(span);
        };
        let tag = match &elem {
            Type::Object(name) => self
                .types
                .get(name)
                .map(|info| object_elem_tag(info.type_id))
                .expect("matrix object element type was resolved before construction"),
            _ => registry::scalar_type_tag(&elem)
                .expect("matrix element type is always a supported scalar or object"),
        };
        let qualifier = [&rows, &cols, &initial]
            .iter()
            .fold(Qualifier::Const, |q, a| q.join(a.qualifier))
            .join(Qualifier::Series);
        HirExpr {
            kind: HirExprKind::CallCollection {
                func: CollectionFunc::MatrixNew,
                args: vec![rows, cols, initial, Self::const_int_expr(tag, span)],
            },
            ty: Type::Matrix(Box::new(elem)),
            qualifier,
            span,
        }
    }

    /// Check an analyzed `matrix.new` dimension (`rows` or `columns`) against
    /// `int`, reporting `E0234` that names the parameter rather than calling it
    /// an element. `na` adopts `int` and there is no float → int coercion, so
    /// an accepted dimension lowers exactly as an `int` element argument does.
    fn coerce_matrix_dimension(
        &mut self,
        param: &str,
        hir: HirExpr,
        span: Span,
    ) -> Option<HirExpr> {
        if hir.ty.is_error() {
            return None;
        }
        if matches!(hir.kind, HirExprKind::ConstNa) {
            return Some(HirExpr {
                ty: Type::Int,
                ..hir
            });
        }
        if hir.ty == Type::Int {
            return Some(hir);
        }
        self.error(
            codes::E0234,
            format!(
                "`{}` {param} must be int, found {}",
                CollectionFunc::MatrixNew.name(),
                hir.ty.describe()
            ),
            span,
            "expected an int",
        );
        None
    }

    /// The constructor's third parameter is `initial_value`. The binder's
    /// did-you-mean only reaches names within two edits of a parameter, so
    /// attach a hint naming `initial_value` to any `E0228` for the spelling
    /// `initial` reported since `first_new_diagnostic`.
    fn note_renamed_matrix_initial(
        &mut self,
        callee: &str,
        first_new_diagnostic: pinewasm_span::DiagnosticsMark,
    ) {
        let legacy = format!("`{callee}` has no parameter named `initial`");
        for diagnostic in self.diagnostics.kept_since_mut(first_new_diagnostic) {
            if diagnostic.code.as_deref() == Some(codes::E0228) && diagnostic.message == legacy {
                diagnostic
                    .notes
                    .push("did you mean `initial_value`?".to_owned());
            }
        }
    }

    /// The receiver-taking `matrix.*` operations. The receiver (first argument)
    /// must be a `matrix<T>`; argument and result types derive from `T` and the
    /// operation. Range/index defaults are materialized here so the emitted CSIL
    /// is fully explicit (mirroring `map.*`).
    pub(crate) fn analyze_matrix_operation(
        &mut self,
        func: CollectionFunc,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        use CollectionFunc as F;
        if args.is_empty() {
            self.arity_error(func, "at least 1", args, span);
            return self.error_expr(span);
        }
        let recv = self.analyze_expr(&args[0].value);
        if recv.ty.is_error() {
            for a in &args[1..] {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        }
        let elem_ty = match recv.ty.matrix_elem() {
            Some(t) => t.clone(),
            None => {
                self.error(
                    codes::E0235,
                    format!(
                        "`{}` expects a matrix receiver, found {}",
                        func.name(),
                        recv.ty.describe()
                    ),
                    args[0].value.span(),
                    "the first argument must be a matrix",
                );
                for a in &args[1..] {
                    let _ = self.analyze_expr(&a.value);
                }
                return self.error_expr(span);
            }
        };
        let rest = &args[1..];
        let mat_ty = Type::Matrix(Box::new(elem_ty.clone()));
        let float_mat_ty = Type::Matrix(Box::new(Type::Float));
        // Whether the numeric-only operations are allowed on this element type.
        // Linear algebra / aggregates require int or float cells.
        let is_numeric = matches!(elem_ty, Type::Int | Type::Float);

        // (surface non-receiver arg count, result type). `want` is the surface
        // count; some ops materialize extra CSIL args below.
        let (want, result): (usize, Type) = match func {
            F::MatrixRows | F::MatrixColumns | F::MatrixElementsCount | F::MatrixRank => {
                (0, Type::Int)
            }
            // Structural line operations preserve the receiver's element
            // type. Removal mutates the receiver and returns the cells it
            // detached; row / col return a fresh shallow array copy.
            F::MatrixRow | F::MatrixCol | F::MatrixRemoveRow | F::MatrixRemoveCol => {
                (1, Type::Array(Box::new(elem_ty.clone())))
            }
            // All three operations compute in f64, including for matrix<int>
            // receivers; the VM allocates float-valued results.
            F::MatrixEigenvalues => (0, Type::Array(Box::new(Type::Float))),
            F::MatrixEigenvectors | F::MatrixPinv => (0, float_mat_ty.clone()),
            // sort mutates its receiver and returns void; column/order are
            // optional (materialized below), so validate the surface count in
            // the arm.
            F::MatrixSort => (rest.len(), Type::Void),
            F::MatrixCopy | F::MatrixTranspose => (0, mat_ty.clone()),
            // reverse mutates the receiver in place and returns nothing, unlike
            // copy/transpose which build a new matrix, so binding its result is
            // rejected.
            F::MatrixReverse => (0, Type::Void),
            F::MatrixGet => (2, elem_ty.clone()),
            F::MatrixSet => (3, Type::Void),
            F::MatrixSwapRows | F::MatrixSwapColumns | F::MatrixReshape => (2, Type::Void),
            // add_row/add_col: (index[, values]); both are optional and
            // validated in the arm below.
            F::MatrixAddRow | F::MatrixAddCol => (rest.len(), Type::Void),
            // fill: (value[, from_row, to_row, from_column, to_column]): the
            // four bounds are individually optional, so accept the surface
            // count and validate its range in the arm below (as `MatrixSort`).
            F::MatrixFill => (rest.len(), Type::Void),
            F::MatrixSubmatrix => (4, mat_ty.clone()),
            F::MatrixConcat => (1, mat_ty.clone()),
            F::MatrixSum | F::MatrixDiff | F::MatrixMult => (1, float_mat_ty.clone()),
            F::MatrixKron => (1, float_mat_ty.clone()),
            F::MatrixPow => (1, float_mat_ty.clone()),
            F::MatrixInv => (0, float_mat_ty),
            F::MatrixDet | F::MatrixTrace => (0, Type::Float),
            F::MatrixAvg | F::MatrixMin | F::MatrixMax | F::MatrixMedian | F::MatrixMode => {
                (0, Type::Float)
            }
            F::MatrixIsSquare
            | F::MatrixIsIdentity
            | F::MatrixIsDiagonal
            | F::MatrixIsSymmetric
            | F::MatrixIsAntisymmetric
            | F::MatrixIsTriangular
            | F::MatrixIsStochastic
            | F::MatrixIsBinary
            | F::MatrixIsZero
            | F::MatrixIsAntidiagonal => (0, Type::Bool),
            _ => unreachable!("non-matrix function routed to analyze_matrix_operation"),
        };

        // Numeric-only guard for linear algebra, aggregates, and numeric
        // predicates. Structural ops (get/set/copy/shape/…) work for any T.
        let needs_numeric = matches!(
            func,
            F::MatrixSum
                | F::MatrixDiff
                | F::MatrixMult
                | F::MatrixKron
                | F::MatrixPow
                | F::MatrixDet
                | F::MatrixInv
                | F::MatrixTrace
                | F::MatrixAvg
                | F::MatrixMin
                | F::MatrixMax
                | F::MatrixMedian
                | F::MatrixMode
                | F::MatrixIsIdentity
                | F::MatrixIsDiagonal
                | F::MatrixIsSymmetric
                | F::MatrixIsAntisymmetric
                | F::MatrixIsTriangular
                | F::MatrixIsStochastic
                | F::MatrixIsBinary
                | F::MatrixIsZero
                | F::MatrixIsAntidiagonal
                | F::MatrixRank
                | F::MatrixEigenvalues
                | F::MatrixEigenvectors
                | F::MatrixPinv
        );
        if needs_numeric && !is_numeric {
            self.error(
                codes::E0234,
                format!(
                    "`{}` requires a numeric (int or float) matrix, found {}",
                    func.name(),
                    recv.ty.describe()
                ),
                args[0].value.span(),
                "this operation is only defined for int or float matrices",
            );
            for a in rest {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        }

        if func == F::MatrixSort
            && !matches!(
                elem_ty,
                Type::Int | Type::Float | Type::String | Type::Object(_)
            )
        {
            self.error(
                codes::E0234,
                format!(
                    "`{}` requires an int, float, string, or user-defined type matrix, found {}",
                    func.name(),
                    recv.ty.describe()
                ),
                args[0].value.span(),
                "matrices of this element type cannot be sorted",
            );
            for a in rest {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        }

        if rest.len() != want {
            self.arity_error(func, &format!("{}", want + 1), args, span);
            return self.error_expr(span);
        }

        let mut final_args = vec![recv];
        match func {
            // Unary (id).
            F::MatrixRows
            | F::MatrixColumns
            | F::MatrixElementsCount
            | F::MatrixCopy
            | F::MatrixTranspose
            | F::MatrixReverse
            | F::MatrixInv
            | F::MatrixDet
            | F::MatrixTrace
            | F::MatrixAvg
            | F::MatrixMin
            | F::MatrixMax
            | F::MatrixMedian
            | F::MatrixMode
            | F::MatrixIsSquare
            | F::MatrixIsIdentity
            | F::MatrixIsDiagonal
            | F::MatrixIsSymmetric
            | F::MatrixIsAntisymmetric
            | F::MatrixIsTriangular
            | F::MatrixIsStochastic
            | F::MatrixIsBinary
            | F::MatrixIsZero
            | F::MatrixIsAntidiagonal
            | F::MatrixEigenvalues
            | F::MatrixEigenvectors
            | F::MatrixPinv
            | F::MatrixRank => {}
            // sort: column and order default on all matrices; a UDT matrix may
            // also supply a const field index or name for the object key.
            F::MatrixSort => {
                let is_udt = matches!(elem_ty, Type::Object(_));
                let max_rest = if is_udt { 3 } else { 2 };
                if rest.len() > max_rest {
                    self.arity_error(func, if is_udt { "1 to 4" } else { "1 to 3" }, args, span);
                    return self.error_expr(span);
                }
                let column = if let Some(arg) = rest.first() {
                    match self.coerce_int_arg(func, arg) {
                        Some(e) => e,
                        None => return self.error_expr(span),
                    }
                } else {
                    Self::const_int_expr(0, span)
                };
                let order = if let Some(arg) = rest.get(1) {
                    match self.coerce_int_arg(func, arg) {
                        Some(e) => e,
                        None => return self.error_expr(span),
                    }
                } else {
                    Self::const_int_expr(0, span)
                };
                final_args.push(column);
                final_args.push(order);

                if let Type::Object(type_name) = &elem_ty {
                    let sort_field = if let Some(arg) = rest.get(2) {
                        let field = self.analyze_expr(&arg.value);
                        if field.qualifier != Qualifier::Const {
                            self.error(
                                codes::E0234,
                                "`matrix.sort` argument `sort_field` must be const".to_string(),
                                arg.value.span(),
                                "use a const int field index or const string field name",
                            );
                            return self.error_expr(span);
                        }
                        let value = self.const_input_value_from_hir(&field);
                        match (&field.ty, value) {
                            (Type::Int, Some(InputValue::Int(index))) => {
                                match usize::try_from(index) {
                                    Ok(index) => index,
                                    Err(_) => {
                                        self.error(
                                            codes::E0234,
                                            "`matrix.sort` sort_field index must be non-negative"
                                                .to_string(),
                                            arg.value.span(),
                                            "the first field has index 0",
                                        );
                                        return self.error_expr(span);
                                    }
                                }
                            }
                            (Type::String, Some(InputValue::String(name))) => {
                                let Some((index, _)) =
                                    self.types.get(type_name).and_then(|info| info.field(&name))
                                else {
                                    self.error(
                                        codes::E0234,
                                        format!("`{name}` is not a field of `{type_name}`"),
                                        arg.value.span(),
                                        "use the exact identifier of a declared field",
                                    );
                                    return self.error_expr(span);
                                };
                                index
                            }
                            (Type::Int, _) => {
                                self.error(
                                    codes::E0234,
                                    "`matrix.sort` sort_field must be a const int field index"
                                        .to_string(),
                                    arg.value.span(),
                                    "use a non-negative const int or a const string field name",
                                );
                                return self.error_expr(span);
                            }
                            (Type::String, _) => {
                                self.error(
                                    codes::E0234,
                                    "`matrix.sort` sort_field must be a const string field name"
                                        .to_string(),
                                    arg.value.span(),
                                    "use a const string that exactly matches a declared field",
                                );
                                return self.error_expr(span);
                            }
                            _ => {
                                self.error(
                                    codes::E0234,
                                    format!(
                                        "`matrix.sort` sort_field must be const int or const string, found {}",
                                        field.ty.describe()
                                    ),
                                    arg.value.span(),
                                    "use a const int field index or const string field name",
                                );
                                return self.error_expr(span);
                            }
                        }
                    } else {
                        0
                    };
                    let Some(field) = self
                        .types
                        .get(type_name)
                        .and_then(|info| info.fields.get(sort_field))
                    else {
                        self.error(
                            codes::E0234,
                            format!("`matrix.sort` field index {sort_field} is out of range for `{type_name}`"),
                            span,
                            "the index must identify a declared field",
                        );
                        return self.error_expr(span);
                    };
                    if !matches!(field.ty, Type::Int | Type::Float | Type::String) {
                        self.error(
                            codes::E0234,
                            format!(
                                "`matrix.sort` cannot sort `{type_name}` by `{}` because it has type {}",
                                field.name,
                                field.ty.describe()
                            ),
                            span,
                            "sort fields must be int, float, or string",
                        );
                        return self.error_expr(span);
                    }
                    final_args.push(Self::const_int_expr(sort_field as i64, span));
                } else if rest.len() > 2 {
                    self.error(
                        codes::E0235,
                        "`matrix.sort` parameter `sort_field` is only valid for matrices of user-defined types".to_string(),
                        rest[2].value.span(),
                        "primitive matrices are sorted by the values in the selected column",
                    );
                    return self.error_expr(span);
                } else {
                    final_args.push(Self::const_int_expr(0, span));
                }
            }
            // (index:int) — row / col line extraction.
            F::MatrixRow | F::MatrixCol => match self.coerce_int_arg(func, &rest[0]) {
                Some(e) => final_args.push(e),
                None => return self.error_expr(span),
            },
            // (row:int, column:int).
            F::MatrixGet | F::MatrixSwapRows | F::MatrixSwapColumns | F::MatrixReshape => {
                for a in rest {
                    match self.coerce_int_arg(func, a) {
                        Some(e) => final_args.push(e),
                        None => return self.error_expr(span),
                    }
                }
            }
            // (index:int).
            F::MatrixRemoveRow | F::MatrixRemoveCol => match self.coerce_int_arg(func, &rest[0]) {
                Some(e) => final_args.push(e),
                None => return self.error_expr(span),
            },
            // (row, column, value).
            F::MatrixSet => {
                match self.coerce_int_arg(func, &rest[0]) {
                    Some(e) => final_args.push(e),
                    None => return self.error_expr(span),
                }
                match self.coerce_int_arg(func, &rest[1]) {
                    Some(e) => final_args.push(e),
                    None => return self.error_expr(span),
                }
                let v = self.analyze_expr_expecting(&rest[2].value, Some(&elem_ty));
                match self.coerce_element_arg(func, &elem_ty, v, rest[2].value.span()) {
                    Some(e) => final_args.push(e),
                    None => return self.error_expr(span),
                }
            }
            // (index:int, values: array<T>).
            // Omitted, the index appends and the array leaves the new line
            // `na`; both reach the VM as `na`.
            F::MatrixAddRow | F::MatrixAddCol => {
                if rest.len() > 2 {
                    self.arity_error(func, "1 to 3", args, span);
                    return self.error_expr(span);
                }
                match rest.first() {
                    Some(a) => match self.coerce_int_arg(func, a) {
                        Some(e) => final_args.push(e),
                        None => return self.error_expr(span),
                    },
                    None => final_args.push(HirExpr {
                        kind: HirExprKind::ConstNa,
                        ty: Type::Int,
                        qualifier: Qualifier::Const,
                        span,
                    }),
                }
                let want_arr = Type::Array(Box::new(elem_ty.clone()));
                match rest.get(1) {
                    None => final_args.push(HirExpr {
                        kind: HirExprKind::ConstNa,
                        ty: want_arr,
                        qualifier: Qualifier::Const,
                        span,
                    }),
                    Some(values) => {
                        let arr = self.analyze_expr_expecting(&values.value, Some(&want_arr));
                        if arr.ty.is_error() {
                            return self.error_expr(span);
                        }
                        if arr.ty != want_arr {
                            self.error(
                                codes::E0234,
                                format!(
                                    "`{}` expects an {}, found {}",
                                    func.name(),
                                    want_arr.describe(),
                                    arr.ty.describe()
                                ),
                                values.value.span(),
                                format!("the inserted line must be an {}", want_arr.describe()),
                            );
                            return self.error_expr(span);
                        }
                        final_args.push(arr);
                    }
                }
            }
            // fill: (value[, from_row, to_row, from_column, to_column]). Each
            // omitted bound is materialized as its sentinel (`0` for a start,
            // `-1` for an end); the VM reads -1 as "to the last row/column".
            F::MatrixFill => {
                if !(1..=5).contains(&rest.len()) {
                    self.arity_error(func, "2 to 6", args, span);
                    return self.error_expr(span);
                }
                let v = self.analyze_expr_expecting(&rest[0].value, Some(&elem_ty));
                match self.coerce_element_arg(func, &elem_ty, v, rest[0].value.span()) {
                    Some(e) => final_args.push(e),
                    None => return self.error_expr(span),
                }
                for (i, sentinel) in [0, -1, 0, -1].into_iter().enumerate() {
                    match rest.get(i + 1) {
                        Some(a) => match self.coerce_int_arg(func, a) {
                            Some(e) => final_args.push(e),
                            None => return self.error_expr(span),
                        },
                        None => final_args.push(Self::const_int_expr(sentinel, span)),
                    }
                }
            }
            // submatrix: (from_row, to_row, from_column, to_column).
            F::MatrixSubmatrix => {
                for a in rest {
                    match self.coerce_int_arg(func, a) {
                        Some(e) => final_args.push(e),
                        None => return self.error_expr(span),
                    }
                }
            }
            // (other: matrix<same T>).
            F::MatrixConcat | F::MatrixKron => {
                let other = self.analyze_expr(&rest[0].value);
                if other.ty.is_error() {
                    return self.error_expr(span);
                }
                if other.ty != mat_ty {
                    self.error(
                        codes::E0234,
                        format!(
                            "`{}` requires two matrices of the same element type, found {} and {}",
                            func.name(),
                            mat_ty.describe(),
                            other.ty.describe()
                        ),
                        rest[0].value.span(),
                        "both matrices must share their element type",
                    );
                    return self.error_expr(span);
                }
                final_args.push(other);
            }
            // (matrix<T> | scalar). Elementwise with a matrix, or broadcast a
            // scalar. `mult` additionally accepts a matrix product.
            F::MatrixSum | F::MatrixDiff | F::MatrixMult => {
                let other = self.analyze_expr_expecting(&rest[0].value, Some(&elem_ty));
                if other.ty.is_error() {
                    return self.error_expr(span);
                }
                let ok = other.ty == mat_ty
                    || other.ty == elem_ty
                    || (elem_ty == Type::Float && other.ty == Type::Int);
                if !ok {
                    self.error(
                        codes::E0234,
                        format!(
                            "`{}` second operand must be a {} or a {} scalar, found {}",
                            func.name(),
                            mat_ty.describe(),
                            elem_ty.describe(),
                            other.ty.describe()
                        ),
                        rest[0].value.span(),
                        "pass a matrix of the same element type or a scalar",
                    );
                    return self.error_expr(span);
                }
                // Widen an int scalar for a float matrix.
                let other = if elem_ty == Type::Float && other.ty == Type::Int {
                    Self::coerce_int_to_float(other)
                } else {
                    other
                };
                final_args.push(other);
            }
            // (power:int).
            F::MatrixPow => match self.coerce_int_arg(func, &rest[0]) {
                Some(e) => final_args.push(e),
                None => return self.error_expr(span),
            },
            _ => unreachable!("non-matrix function routed to analyze_matrix_operation"),
        }

        let qualifier = final_args
            .iter()
            .fold(Qualifier::Const, |q, a| q.join(a.qualifier))
            .join(Qualifier::Series);
        HirExpr {
            kind: HirExprKind::CallCollection {
                func,
                args: final_args,
            },
            ty: result,
            qualifier,
            span,
        }
    }

    /// Analyze an argument expected to be an `int` (a matrix index / count),
    /// emitting `E0234` on a non-int. `na` adopts int; no float→int coercion.
    pub(crate) fn coerce_int_arg(
        &mut self,
        func: CollectionFunc,
        arg: &ast::CallArg,
    ) -> Option<HirExpr> {
        let hir = self.analyze_expr_expecting(&arg.value, Some(&Type::Int));
        self.coerce_element_arg(func, &Type::Int, hir, arg.value.span())
    }

    /// The receiver-taking `map.*` operations. The receiver (first argument)
    /// must be a `map<K, V>`; argument and result types derive from `K`/`V`.
    pub(crate) fn analyze_map_operation(
        &mut self,
        func: CollectionFunc,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        use CollectionFunc as F;
        if args.is_empty() {
            self.arity_error(func, "at least 1", args, span);
            return self.error_expr(span);
        }
        let recv = self.analyze_expr(&args[0].value);
        if recv.ty.is_error() {
            for a in &args[1..] {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        }
        let (key_ty, val_ty) = match recv.ty.map_types() {
            Some((k, v)) => (k.clone(), v.clone()),
            None => {
                self.error(
                    codes::E0235,
                    format!(
                        "`{}` expects a map receiver, found {}",
                        func.name(),
                        recv.ty.describe()
                    ),
                    args[0].value.span(),
                    "the first argument must be a map",
                );
                for a in &args[1..] {
                    let _ = self.analyze_expr(&a.value);
                }
                return self.error_expr(span);
            }
        };
        let rest = &args[1..];
        // (non-receiver arg count, result type).
        let (want, result): (usize, Type) = match func {
            F::MapSize => (0, Type::Int),
            F::MapClear => (0, Type::Void),
            F::MapKeys => (0, Type::Array(Box::new(key_ty.clone()))),
            F::MapValues => (0, Type::Array(Box::new(val_ty.clone()))),
            F::MapCopy => (
                0,
                Type::Map(Box::new(key_ty.clone()), Box::new(val_ty.clone())),
            ),
            F::MapContains => (1, Type::Bool),
            // get / remove return the value type (or na).
            F::MapGet | F::MapRemove => (1, val_ty.clone()),
            // put returns the previous value (or na).
            F::MapPut => (2, val_ty.clone()),
            F::MapPutAll => (1, Type::Void),
            _ => unreachable!("non-map function routed to analyze_map_operation"),
        };
        if rest.len() != want {
            self.arity_error(func, &format!("{}", want + 1), args, span);
            return self.error_expr(span);
        }
        let mut final_args = vec![recv];
        match func {
            F::MapSize | F::MapClear | F::MapKeys | F::MapValues | F::MapCopy => {}
            // (key: K).
            F::MapContains | F::MapGet | F::MapRemove => {
                let k = self.analyze_expr_expecting(&rest[0].value, Some(&key_ty));
                match self.coerce_element_arg(func, &key_ty, k, rest[0].value.span()) {
                    Some(e) => final_args.push(e),
                    None => return self.error_expr(span),
                }
            }
            // (key: K, value: V).
            F::MapPut => {
                let k = self.analyze_expr_expecting(&rest[0].value, Some(&key_ty));
                match self.coerce_element_arg(func, &key_ty, k, rest[0].value.span()) {
                    Some(e) => final_args.push(e),
                    None => return self.error_expr(span),
                }
                let v = self.analyze_expr_expecting(&rest[1].value, Some(&val_ty));
                match self.coerce_element_arg(func, &val_ty, v, rest[1].value.span()) {
                    Some(e) => final_args.push(e),
                    None => return self.error_expr(span),
                }
            }
            // (other: map<same K, same V>).
            F::MapPutAll => {
                let other = self.analyze_expr(&rest[0].value);
                if other.ty.is_error() {
                    return self.error_expr(span);
                }
                let want_ty = Type::Map(Box::new(key_ty.clone()), Box::new(val_ty.clone()));
                if other.ty != want_ty {
                    self.error(
                        codes::E0234,
                        format!(
                            "`map.put_all` requires two maps of the same key/value types, found {} and {}",
                            want_ty.describe(),
                            other.ty.describe()
                        ),
                        rest[0].value.span(),
                        "both maps must share key and value types",
                    );
                    return self.error_expr(span);
                }
                final_args.push(other);
            }
            _ => unreachable!("non-map function routed to analyze_map_operation"),
        }
        let qualifier = final_args
            .iter()
            .fold(Qualifier::Const, |q, a| q.join(a.qualifier))
            .join(Qualifier::Series);
        HirExpr {
            kind: HirExprKind::CallCollection {
                func,
                args: final_args,
            },
            ty: result,
            qualifier,
            span,
        }
    }

    /// The element type a typed `array.new_*` constructor produces.
    pub(crate) fn constructor_elem_type(func: CollectionFunc) -> Type {
        match func {
            CollectionFunc::ArrayNewFloat => Type::Float,
            CollectionFunc::ArrayNewInt => Type::Int,
            CollectionFunc::ArrayNewBool => Type::Bool,
            CollectionFunc::ArrayNewString => Type::String,
            CollectionFunc::ArrayNewColor => Type::Color,
            _ => Type::Error,
        }
    }

    /// A generic constructor whose type arguments name a user-defined type or
    /// an enum: `array.new<MyType>(...)`, `matrix.new<MyType>(...)`,
    /// `map.new<key, MyType>()`, and the same forms with `MyEnum` (as a map
    /// key too: `map.new<MyEnum, float>()`). The parser keeps the base name as
    /// the callee and carries the type arguments structurally on the call
    /// node, with no in-band string encoding. Validates that each named type
    /// is a known object or enum type (`E0234` otherwise) and builds the
    /// corresponding collection constructor. An enum value is its variant
    /// index at runtime, so an enum element or key uses the int lane while
    /// the HIR type stays the nominal enum.
    pub(crate) fn analyze_object_collection_ctor(
        &mut self,
        type_args: &[ast::Ident],
        args: &[ast::CallArg],
        callee: &ast::Ident,
        span: Span,
    ) -> HirExpr {
        let base = callee.name.as_str();
        let resolved_names: Vec<String> = type_args
            .iter()
            .map(|arg| {
                self.object_type_name(&arg.name)
                    .filter(|name| self.types.contains_key(name))
                    .or_else(|| self.enum_type_name(&arg.name))
                    .unwrap_or_else(|| arg.name.clone())
            })
            .collect();
        let type_args: Vec<&str> = resolved_names.iter().map(String::as_str).collect();
        // Resolve a single named element type to its `type_id`, reporting
        // `E0234` for a name that is not a known object type.
        let resolve_udt = |this: &mut Self, name: &str| -> Option<u32> {
            match this.types.get(name) {
                Some(info) => Some(info.type_id),
                None => {
                    let mut diag = Diagnostic::error(
                        codes::E0234,
                        format!("unknown element type `{name}` in generic constructor"),
                    )
                    .with_label(
                        callee.span,
                        "not a scalar type, a user-defined type, or an enum",
                    );
                    let (types, enums) = (&this.types, &this.enums);
                    if let Some(s) = this.suggestions.among(name, || {
                        types
                            .keys()
                            .chain(enums.keys())
                            .map(|known| Some(known.as_str()))
                    }) {
                        diag = diag.with_note(format!("did you mean `{s}`?"));
                    }
                    this.diagnostics.push(diag);
                    None
                }
            }
        };
        // The enum a resolved type argument names, if any.
        let enum_type = |this: &Self, name: &str| -> Option<Type> {
            this.enums
                .contains_key(name)
                .then(|| Type::Enum(name.to_string()))
        };

        match base {
            "array.new" => {
                // Exactly one type argument (an object or enum element type).
                let elem_name = type_args[0];
                if let Some(elem) = enum_type(self, elem_name) {
                    return self.analyze_typed_array_constructor(
                        CollectionFunc::ArrayNewInt,
                        &format!("array.new<{elem_name}>"),
                        elem,
                        args,
                        span,
                    );
                }
                let Some(type_id) = resolve_udt(self, elem_name) else {
                    for a in args {
                        let _ = self.analyze_expr(&a.value);
                    }
                    return self.error_expr(span);
                };
                self.analyze_object_array_constructor(type_id, elem_name, args, span)
            }
            "matrix.new" => {
                if type_args.len() != 1 {
                    self.error(
                        codes::E0234,
                        "`matrix.new` expects one user-defined type argument".to_string(),
                        callee.span,
                        "expected `matrix.new<MyType>(rows, columns, initial_value)`",
                    );
                    for a in args {
                        let _ = self.analyze_expr(&a.value);
                    }
                    return self.error_expr(span);
                }
                let elem_name = type_args[0];
                if let Some(elem) = enum_type(self, elem_name) {
                    return self.analyze_matrix_constructor(elem, args, span);
                }
                if resolve_udt(self, elem_name).is_none() {
                    for a in args {
                        let _ = self.analyze_expr(&a.value);
                    }
                    return self.error_expr(span);
                }
                self.analyze_matrix_constructor(Type::Object(elem_name.to_owned()), args, span)
            }
            "map.new" => {
                // Two type arguments: a scalar or enum key, and a scalar,
                // enum, or object value (at least one of them is not scalar,
                // or the parser would have rewritten the call).
                if type_args.len() != 2 {
                    self.error(
                        codes::E0234,
                        "`map.new` expects two type arguments".to_string(),
                        callee.span,
                        "expected `map.new<key, value>`",
                    );
                    for a in args {
                        let _ = self.analyze_expr(&a.value);
                    }
                    return self.error_expr(span);
                }
                let key_name = type_args[0];
                let val_name = type_args[1];
                // The key must be a scalar or enum type.
                let Some(key_ty) =
                    scalar_type_from_name(key_name).or_else(|| enum_type(self, key_name))
                else {
                    self.error(
                        codes::E0234,
                        format!("map key type `{key_name}` must be a scalar or enum type"),
                        callee.span,
                        "map keys are int, float, bool, string, color, or an enum",
                    );
                    for a in args {
                        let _ = self.analyze_expr(&a.value);
                    }
                    return self.error_expr(span);
                };
                if let Some(val_ty) =
                    scalar_type_from_name(val_name).or_else(|| enum_type(self, val_name))
                {
                    return self.analyze_map_constructor(key_ty, val_ty, args, span);
                }
                let Some(type_id) = resolve_udt(self, val_name) else {
                    for a in args {
                        let _ = self.analyze_expr(&a.value);
                    }
                    return self.error_expr(span);
                };
                self.analyze_object_map_constructor(key_ty, type_id, val_name, args, span)
            }
            // Any other base. Unreachable from parsed source: the parser
            // attaches type arguments only to the three bases above.
            _ => {
                self.error(
                    codes::E0234,
                    format!("`{base}<...>` does not support user-defined element types"),
                    callee.span,
                    "only `array`, `matrix`, and `map` may hold user-defined types",
                );
                for a in args {
                    let _ = self.analyze_expr(&a.value);
                }
                self.error_expr(span)
            }
        }
    }

    /// Build `array.new<MyType>(size = 0, initial = na)`. The emitted
    /// `CallCollection` carries `(size, initial, type_tag)`.
    pub(crate) fn analyze_object_array_constructor(
        &mut self,
        type_id: u32,
        type_name: &str,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let Some(args) =
            self.bind_array_constructor_args(&format!("array.new<{type_name}>"), args, span)
        else {
            return self.error_expr(span);
        };
        let args = &args[..];
        if args.len() > 2 {
            self.error(
                codes::E0204,
                format!(
                    "`array.new<{type_name}>` expects 0 to 2 arguments, found {}",
                    args.len()
                ),
                span,
                "expected (size[, initial])",
            );
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        }
        let obj_ty = Type::Object(type_name.to_string());
        let mut ok = true;

        // size argument (int, default 0).
        let size_expr = if let Some(arg) = args.first() {
            let hir = self.analyze_expr(&arg.value);
            if !hir.ty.is_error() && hir.ty != Type::Int {
                self.error(
                    codes::E0234,
                    format!(
                        "`array.new<{type_name}>` size must be int, found {}",
                        hir.ty.describe()
                    ),
                    arg.value.span(),
                    "expected an int size",
                );
                ok = false;
            }
            hir
        } else {
            Self::const_int_expr(0, span)
        };

        // initial argument (an object of the element type, or na; default na).
        let initial_expr = if let Some(arg) = args.get(1) {
            let hir = self.analyze_expr_expecting(&arg.value, Some(&obj_ty));
            if !hir.ty.is_error() && hir.ty != obj_ty {
                self.error(
                    codes::E0234,
                    format!(
                        "`array.new<{type_name}>` initial must be `{type_name}` or na, found {}",
                        hir.ty.describe()
                    ),
                    arg.value.span(),
                    format!("expected a `{type_name}` value"),
                );
                ok = false;
            }
            hir
        } else {
            HirExpr {
                kind: HirExprKind::ConstNa,
                ty: obj_ty.clone(),
                qualifier: Qualifier::Const,
                span,
            }
        };

        if !ok {
            return self.error_expr(span);
        }
        let type_tag = object_elem_tag(type_id);
        let qualifier = size_expr
            .qualifier
            .join(initial_expr.qualifier)
            .join(Qualifier::Series);
        HirExpr {
            kind: HirExprKind::CallCollection {
                func: CollectionFunc::ArrayNewObject,
                args: vec![
                    size_expr,
                    initial_expr,
                    Self::const_int_expr(type_tag, span),
                ],
            },
            ty: Type::Array(Box::new(obj_ty)),
            qualifier,
            span,
        }
    }

    /// Build `map.new<key, MyType>()` — a fresh empty `map<key, MyType>`. The
    /// emitted `MapNew` carries the key scalar tag and the object value tag.
    pub(crate) fn analyze_object_map_constructor(
        &mut self,
        key_ty: Type,
        val_type_id: u32,
        val_name: &str,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        for arg in args {
            let _ = self.analyze_expr(&arg.value);
        }
        if !args.is_empty() {
            self.error(
                codes::E0204,
                format!(
                    "`map.new<{}, {val_name}>` expects 0 arguments, found {}",
                    key_ty.describe(),
                    args.len()
                ),
                span,
                "map constructors take no arguments",
            );
            return self.error_expr(span);
        }
        let key_tag =
            registry::scalar_type_tag(&key_ty).expect("map key type is always a scalar value type");
        let val_tag = object_elem_tag(val_type_id);
        HirExpr {
            kind: HirExprKind::CallCollection {
                func: CollectionFunc::MapNew,
                args: vec![
                    Self::const_int_expr(key_tag, span),
                    Self::const_int_expr(val_tag, span),
                ],
            },
            ty: Type::Map(
                Box::new(key_ty),
                Box::new(Type::Object(val_name.to_string())),
            ),
            qualifier: Qualifier::Series,
            span,
        }
    }

    /// Bind the arguments of an `array.new*` constructor, `(size = 0,
    /// initial_value)`, by their Pine names, as the scalar constructors do in
    /// [`Self::analyze_collection_call`]. The typed (`array.new<MyEnum>`) and
    /// object (`array.new<MyType>`, `array.new_line`) forms do not pass
    /// through that call, so without this a named argument was consumed in
    /// written order (`initial_value = Side.sell, size = 2` took `Side.sell`
    /// as the size) and an unknown name was ignored. `None` when the binding
    /// is rejected (the diagnostic is already reported).
    fn bind_array_constructor_args<'a>(
        &mut self,
        name: &str,
        args: &'a [ast::CallArg],
        span: Span,
    ) -> Option<std::borrow::Cow<'a, [ast::CallArg]>> {
        if args.iter().all(|arg| arg.name.is_none()) {
            return Some(std::borrow::Cow::Borrowed(args));
        }
        let (param_names, _) = CollectionFunc::ArrayNewObject.surface_params();
        let zero = ast::Expr::Int { value: 0, span };
        let defaults = [Some(&zero), None];
        let bound = self.bind_positional_with_defaults(name, param_names, &defaults, args, span)?;
        Some(std::borrow::Cow::Owned(
            bound
                .into_iter()
                .map(|value| ast::CallArg {
                    span: value.span(),
                    name: None,
                    value,
                })
                .collect(),
        ))
    }

    /// `array.new_float(size = 0, initial = na)` and its typed siblings.
    pub(crate) fn analyze_array_constructor(
        &mut self,
        func: CollectionFunc,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        self.analyze_typed_array_constructor(
            func,
            func.name(),
            Self::constructor_elem_type(func),
            args,
            span,
        )
    }

    /// A typed array constructor `name(size = 0, initial = na)` producing an
    /// `array<elem_ty>` through the runtime constructor `func`. `name` is the
    /// spelling diagnostics use. `array.new<MyEnum>()` uses this with
    /// `ArrayNewInt` (an enum value is its variant index at runtime) and the
    /// nominal enum element type, so an `initial` of another type is `E0234`.
    pub(crate) fn analyze_typed_array_constructor(
        &mut self,
        func: CollectionFunc,
        name: &str,
        elem_ty: Type,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        let Some(args) = self.bind_array_constructor_args(name, args, span) else {
            return self.error_expr(span);
        };
        let args = &args[..];
        if args.len() > 2 {
            self.error(
                codes::E0204,
                format!("`{name}` expects 0 to 2 argument(s), found {}", args.len()),
                span,
                "expected 0 to 2 argument(s)",
            );
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        }
        let mut ok = true;

        // size argument (int, default 0).
        let size_expr = if let Some(arg) = args.first() {
            let hir = self.analyze_expr(&arg.value);
            if !hir.ty.is_error() && hir.ty != Type::Int {
                self.error(
                    codes::E0234,
                    format!("`{name}` size must be int, found {}", hir.ty.describe()),
                    arg.value.span(),
                    "expected an int size",
                );
                ok = false;
            }
            hir
        } else {
            Self::const_int_expr(0, span)
        };

        // initial argument (element type, default na; a v6 `bool` is never
        // `na`, so `array.new_bool` and `array.new<bool>` default to false).
        let initial_expr = if let Some(arg) = args.get(1) {
            let hir = self.analyze_expr_expecting(&arg.value, Some(&elem_ty));
            match self.coerce_element_arg_named(name, &elem_ty, hir, arg.value.span()) {
                Some(e) => e,
                None => {
                    ok = false;
                    self.error_expr(span)
                }
            }
        } else if elem_ty == Type::Bool {
            Self::const_bool_expr(false, span)
        } else {
            HirExpr {
                kind: HirExprKind::ConstNa,
                ty: elem_ty.clone(),
                qualifier: Qualifier::Const,
                span,
            }
        };

        if !ok {
            return self.error_expr(span);
        }
        let qualifier = size_expr
            .qualifier
            .join(initial_expr.qualifier)
            // A fresh collection is a distinct mutable object each run: series.
            .join(Qualifier::Series);
        HirExpr {
            kind: HirExprKind::CallCollection {
                func,
                args: vec![size_expr, initial_expr],
            },
            ty: Type::Array(Box::new(elem_ty)),
            qualifier,
            span,
        }
    }

    /// `array.from(v0, v1, ...)` — element type inferred from the args; all
    /// must share one scalar element type (int widening to float when mixed).
    pub(crate) fn analyze_array_from(
        &mut self,
        func: CollectionFunc,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        if args.is_empty() {
            self.arity_error(func, "at least 1", args, span);
            return self.error_expr(span);
        }
        let mut checked: Vec<HirExpr> = Vec::with_capacity(args.len());
        let mut ok = true;
        for arg in args {
            let hir = self.analyze_expr(&arg.value);
            if hir.ty.is_text_format() {
                self.error(
                    codes::E0234,
                    "array elements cannot have built-in text_format type",
                    arg.value.span(),
                    "formatting constants are not string or numeric array elements",
                );
                ok = false;
            }
            if hir.ty.is_plot_display() {
                self.error(
                    codes::E0234,
                    "array elements cannot have built-in plot_display type",
                    arg.value.span(),
                    "display constants are not string or numeric array elements",
                );
                ok = false;
            }
            if hir.ty.is_error() {
                ok = false;
            }
            checked.push(hir);
        }
        if !ok {
            return self.error_expr(span);
        }
        // Infer the element type: the first concrete (non-`na`) argument. Mixed
        // int/float promotes to float. `na`-only is rejected (no inferable
        // type).
        let mut elem_ty: Option<Type> = None;
        let mut any_float = false;
        let mut any_int = false;
        for hir in &checked {
            match &hir.ty {
                Type::Float => any_float = true,
                Type::Int => any_int = true,
                _ => {}
            }
            if !matches!(hir.kind, HirExprKind::ConstNa) && !hir.ty.is_error() {
                match &elem_ty {
                    None => elem_ty = Some(hir.ty.clone()),
                    Some(prev) => {
                        if prev != &hir.ty && !Self::numeric_pair(prev, &hir.ty) {
                            self.error(
                                codes::E0234,
                                format!(
                                    "`array.from` elements must share one type, found {} and {}",
                                    prev.describe(),
                                    hir.ty.describe()
                                ),
                                span,
                                "all elements of an array must have the same type",
                            );
                            return self.error_expr(span);
                        }
                    }
                }
            }
        }
        let elem_ty = match elem_ty {
            Some(t) if any_float && any_int && (t == Type::Int || t == Type::Float) => Type::Float,
            Some(t) => t,
            None => {
                self.error(
                    codes::E0234,
                    "`array.from` cannot infer an element type from all-`na` arguments",
                    span,
                    "provide at least one non-`na` element, or use a typed constructor",
                );
                return self.error_expr(span);
            }
        };
        // Coerce int elements to float when the inferred type is float, and
        // adopt the element type for `na` arguments.
        let mut final_args = Vec::with_capacity(checked.len());
        for hir in checked {
            let coerced = if elem_ty == Type::Float {
                Self::coerce_int_to_float(hir)
            } else {
                hir
            };
            let coerced = if matches!(coerced.kind, HirExprKind::ConstNa) {
                HirExpr {
                    ty: elem_ty.clone(),
                    ..coerced
                }
            } else {
                coerced
            };
            final_args.push(coerced);
        }
        let qualifier = final_args
            .iter()
            .fold(Qualifier::Const, |q, a| q.join(a.qualifier))
            .join(Qualifier::Series);
        HirExpr {
            kind: HirExprKind::CallCollection {
                func,
                args: final_args,
            },
            ty: Type::Array(Box::new(elem_ty)),
            qualifier,
            span,
        }
    }

    /// `str.split(source, separator)` — split `source` on each occurrence of
    /// `separator` into a fresh `array<string>`. Both arguments must be
    /// `string` (`E0234` otherwise). Lives in the `str.*` surface but rides the
    /// collection machinery because it returns a heap array.
    pub(crate) fn analyze_str_split(
        &mut self,
        func: CollectionFunc,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        if args.len() != 2 {
            self.arity_error(func, "2", args, span);
            for a in args {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        }
        let mut ok = true;
        let mut checked = Vec::with_capacity(2);
        for (name, arg) in [("source", &args[0]), ("separator", &args[1])] {
            let hir = self.analyze_expr_expecting(&arg.value, Some(&Type::String));
            if !hir.ty.is_error() && hir.ty != Type::String {
                self.error(
                    codes::E0234,
                    format!(
                        "`str.split` {name} must be string, found {}",
                        hir.ty.describe()
                    ),
                    arg.value.span(),
                    "expected a string",
                );
                ok = false;
            }
            checked.push(hir);
        }
        if !ok {
            return self.error_expr(span);
        }
        let qualifier = checked
            .iter()
            .fold(Qualifier::Const, |q, a| q.join(a.qualifier))
            // A fresh collection is a distinct mutable object each run: series.
            .join(Qualifier::Series);
        HirExpr {
            kind: HirExprKind::CallCollection {
                func,
                args: checked,
            },
            ty: Type::Array(Box::new(Type::String)),
            qualifier,
            span,
        }
    }

    /// The receiver-taking `array.*` operations. The receiver (first argument)
    /// must be an `array<T>`; the result type is derived from `T` per function.
    pub(crate) fn analyze_array_operation(
        &mut self,
        func: CollectionFunc,
        args: &[ast::CallArg],
        span: Span,
    ) -> HirExpr {
        if args.is_empty() {
            self.arity_error(func, "at least 1", args, span);
            return self.error_expr(span);
        }
        // The receiver.
        let recv = self.analyze_expr(&args[0].value);
        if recv.ty.is_error() {
            for a in &args[1..] {
                let _ = self.analyze_expr(&a.value);
            }
            return self.error_expr(span);
        }
        let elem_ty = match recv.ty.array_elem() {
            Some(t) => t.clone(),
            None => {
                self.error(
                    codes::E0235,
                    format!(
                        "`{}` expects an array receiver, found {}",
                        func.name(),
                        recv.ty.describe()
                    ),
                    args[0].value.span(),
                    "the first argument must be an array",
                );
                for a in &args[1..] {
                    let _ = self.analyze_expr(&a.value);
                }
                return self.error_expr(span);
            }
        };

        // Per-function argument checking and result typing.
        let (final_args, ty) = match self.check_array_op_args(func, recv, &elem_ty, args, span) {
            Some(pair) => pair,
            None => return self.error_expr(span),
        };
        let qualifier = final_args
            .iter()
            .fold(Qualifier::Const, |q, a| q.join(a.qualifier))
            .join(Qualifier::Series);
        let result = HirExpr {
            kind: HirExprKind::CallCollection {
                func,
                args: final_args,
            },
            ty,
            qualifier,
            span,
        };
        // The runtime aggregate uses a numeric lane, so an integer min, max
        // or sum is cast back to int to keep its int result.
        if elem_ty == Type::Int
            && matches!(
                func,
                CollectionFunc::ArrayMin | CollectionFunc::ArrayMax | CollectionFunc::ArraySum
            )
        {
            HirExpr {
                kind: HirExprKind::CallPure {
                    func: PureFunc::CastInt,
                    args: vec![result],
                },
                ty: Type::Int,
                qualifier,
                span,
            }
        } else {
            result
        }
    }

    /// Check the trailing arguments of a receiver-taking `array.*` operation
    /// and compute its result type. `recv` is the already-analyzed receiver;
    /// `elem_ty` is its element type. Returns the full (receiver-first)
    /// argument list and the result type, or `None` on a reported error.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn check_array_op_args(
        &mut self,
        func: CollectionFunc,
        recv: HirExpr,
        elem_ty: &Type,
        args: &[ast::CallArg],
        span: Span,
    ) -> Option<(Vec<HirExpr>, Type)> {
        use CollectionFunc as F;
        // Helpers for the common shapes. `rest` are the non-receiver args.
        let rest = &args[1..];
        let int_ty = Type::Int;

        // Expected non-receiver arg count (exact) and result type.
        let (want, result): (usize, Type) = match func {
            F::ArraySize => (0, int_ty.clone()),
            F::ArrayClear | F::ArrayReverse => (0, Type::Void),
            F::ArrayPop | F::ArrayShift | F::ArrayFirst | F::ArrayLast => (0, elem_ty.clone()),
            F::ArrayCopy => (0, Type::Array(Box::new(elem_ty.clone()))),
            F::ArrayPush | F::ArrayUnshift => (1, Type::Void),
            F::ArrayIncludes => (1, Type::Bool),
            F::ArrayIndexOf | F::ArrayLastIndexOf => (1, int_ty.clone()),
            F::ArrayGet => (1, elem_ty.clone()),
            F::ArrayRemove => (1, elem_ty.clone()),
            F::ArraySet | F::ArrayInsert => (2, Type::Void),
            F::ArrayConcat => (1, Type::Array(Box::new(elem_ty.clone()))),
            F::ArraySlice => (2, Type::Array(Box::new(elem_ty.clone()))),
            // fill: (value[, from[, to]]); join: () or (separator).
            F::ArrayFill => (usize::MAX, Type::Void),
            F::ArrayJoin => (usize::MAX, Type::String),
            // Numeric aggregates: no extra args, float result.
            F::ArraySum | F::ArrayAvg | F::ArrayMedian | F::ArrayRange | F::ArrayMode => {
                (0, Type::Float)
            }
            // min/max take an optional `nth`; variance/stdev an optional
            // `biased`. Custom-checked below (the default is materialized).
            F::ArrayMin | F::ArrayMax | F::ArrayVariance | F::ArrayStdev => {
                (usize::MAX, Type::Float)
            }
            // Numeric transforms: no extra args, new array<float>.
            F::ArrayAbs | F::ArrayStandardize => (0, Type::Array(Box::new(Type::Float))),
            // sort: () or (order); void. sort_indices → array<int>.
            F::ArraySort => (usize::MAX, Type::Void),
            F::ArraySortIndices => (usize::MAX, Type::Array(Box::new(int_ty.clone()))),
            // percentrank: (index:int) → float; percentile family:
            // (percentage:float) → float. Exactly one extra arg.
            F::ArrayPercentRank
            | F::ArrayPercentileNearestRank
            | F::ArrayPercentileLinearInterpolation => (1, Type::Float),
            // binary search variants: (val:elem) → int. Exactly one extra arg.
            F::ArrayBinarySearch | F::ArrayBinarySearchLeftmost | F::ArrayBinarySearchRightmost => {
                (1, int_ty.clone())
            }
            // covariance: (other) or (other, biased) → float. Custom-checked.
            F::ArrayCovariance => (usize::MAX, Type::Float),
            // boolean reductions: no extra args, bool result.
            F::ArrayEvery | F::ArraySome => (0, Type::Bool),
            _ => unreachable!("constructors handled elsewhere"),
        };
        if want != usize::MAX && rest.len() != want {
            self.arity_error(func, &format!("{}", want + 1), args, span);
            return None;
        }

        let mut final_args = vec![recv];
        match func {
            // No extra args.
            F::ArraySize
            | F::ArrayClear
            | F::ArrayReverse
            | F::ArrayPop
            | F::ArrayShift
            | F::ArrayFirst
            | F::ArrayLast
            | F::ArrayCopy => {}

            // (value: elem).
            F::ArrayPush
            | F::ArrayUnshift
            | F::ArrayIncludes
            | F::ArrayIndexOf
            | F::ArrayLastIndexOf => {
                let v = self.analyze_expr_expecting(&rest[0].value, Some(elem_ty));
                let v = self.coerce_element_arg(func, elem_ty, v, rest[0].value.span())?;
                final_args.push(v);
            }

            // (index: int).
            F::ArrayGet | F::ArrayRemove => {
                let idx = self.check_int_arg(func, "index", &rest[0])?;
                final_args.push(idx);
            }

            // (index: int, value: elem).
            F::ArraySet | F::ArrayInsert => {
                let idx = self.check_int_arg(func, "index", &rest[0])?;
                final_args.push(idx);
                let v = self.analyze_expr_expecting(&rest[1].value, Some(elem_ty));
                let v = self.coerce_element_arg(func, elem_ty, v, rest[1].value.span())?;
                final_args.push(v);
            }

            // (other: array<same T>).
            F::ArrayConcat => {
                let other = self.analyze_expr(&rest[0].value);
                if other.ty.is_error() {
                    return None;
                }
                if other.ty != Type::Array(Box::new(elem_ty.clone())) {
                    self.error(
                        codes::E0234,
                        format!(
                            "`array.concat` requires two arrays of the same element type, found {} and {}",
                            Type::Array(Box::new(elem_ty.clone())).describe(),
                            other.ty.describe()
                        ),
                        rest[0].value.span(),
                        "both arrays must hold the same element type",
                    );
                    return None;
                }
                final_args.push(other);
            }

            // (from: int, to: int).
            F::ArraySlice => {
                let from = self.check_int_arg(func, "index_from", &rest[0])?;
                let to = self.check_int_arg(func, "index_to", &rest[1])?;
                final_args.push(from);
                final_args.push(to);
            }

            // (value: elem[, from: int[, to: int]]).
            F::ArrayFill => {
                if !(1..=3).contains(&rest.len()) {
                    self.arity_error(func, "2 to 4", args, span);
                    return None;
                }
                let v = self.analyze_expr_expecting(&rest[0].value, Some(elem_ty));
                let v = self.coerce_element_arg(func, elem_ty, v, rest[0].value.span())?;
                final_args.push(v);
                if rest.len() >= 2 {
                    final_args.push(self.check_int_arg(func, "index_from", &rest[1])?);
                }
                if rest.len() == 3 {
                    // `index_to` defaults to `na` (to the end), so a bare `na`
                    // is an int here.
                    let to = &rest[2];
                    let hir = self.analyze_expr_expecting(&to.value, Some(&Type::Int));
                    if hir.ty.is_error() {
                        return None;
                    }
                    if hir.ty != Type::Int {
                        self.error(
                            codes::E0234,
                            format!(
                                "`array.fill` index_to must be int, found {}",
                                hir.ty.describe()
                            ),
                            to.value.span(),
                            "expected an int",
                        );
                        return None;
                    }
                    final_args.push(hir);
                }
            }

            // () or (separator: string). Accepts int, float and string arrays.
            F::ArrayJoin => {
                if rest.len() > 1 {
                    self.arity_error(func, "1 or 2", args, span);
                    return None;
                }
                if !matches!(elem_ty, Type::Int | Type::Float | Type::String) {
                    self.error(
                        codes::E0234,
                        format!(
                            "`array.join` requires an array<int>, array<float> or array<string>, found array<{}>",
                            elem_ty.describe()
                        ),
                        args[0].value.span(),
                        "join applies to numeric and string arrays",
                    );
                    return None;
                }
                if let Some(sep) = rest.first() {
                    let s = self.analyze_expr_expecting(&sep.value, Some(&Type::String));
                    if !s.ty.is_error() && s.ty != Type::String {
                        self.error(
                            codes::E0234,
                            format!(
                                "`array.join` separator must be string, found {}",
                                s.ty.describe()
                            ),
                            sep.value.span(),
                            "expected a string separator",
                        );
                        return None;
                    }
                    final_args.push(s);
                }
            }

            // Numeric aggregates and transforms: require a numeric receiver
            // (`array<int>` or `array<float>`); no extra arguments.
            F::ArraySum
            | F::ArrayAvg
            | F::ArrayMedian
            | F::ArrayRange
            | F::ArrayMode
            | F::ArrayAbs
            | F::ArrayStandardize => {
                self.require_numeric_array(func, elem_ty, args, span)?;
            }

            // min / max: (id) or (id, nth). `nth` selects the nth smallest /
            // largest, defaulting to 0 (the extreme itself).
            F::ArrayMin | F::ArrayMax => {
                self.require_numeric_array(func, elem_ty, args, span)?;
                if rest.len() > 1 {
                    self.arity_error(func, "1 or 2", args, span);
                    return None;
                }
                let nth = match rest.first() {
                    Some(arg) => self.check_int_arg(func, "nth", arg)?,
                    None => Self::const_int_expr(0, span),
                };
                final_args.push(nth);
            }

            // variance / stdev: (id) or (id, biased). `biased` selects the
            // population (n) rather than the sample (n-1) divisor, and defaults
            // to true, matching `array.covariance`.
            F::ArrayVariance | F::ArrayStdev => {
                self.require_numeric_array(func, elem_ty, args, span)?;
                if rest.len() > 1 {
                    self.arity_error(func, "1 or 2", args, span);
                    return None;
                }
                let biased = match rest.first() {
                    Some(arg) => {
                        let b = self.analyze_expr_expecting(&arg.value, Some(&Type::Bool));
                        if b.ty.is_error() {
                            return None;
                        }
                        if b.ty != Type::Bool {
                            self.error(
                                codes::E0234,
                                format!(
                                    "`{}` argument `biased` must be bool, found {}",
                                    func.name(),
                                    b.ty.describe()
                                ),
                                arg.value.span(),
                                "pass `true` or `false`",
                            );
                            return None;
                        }
                        b
                    }
                    None => HirExpr {
                        kind: HirExprKind::ConstBool(true),
                        ty: Type::Bool,
                        qualifier: Qualifier::Const,
                        span,
                    },
                };
                final_args.push(biased);
            }

            // sort: () or (order); sort_indices likewise. The order argument is
            // an int (`order.ascending`/`order.descending`); its default (0 =
            // ascending) is materialized so the emitted call is explicit.
            F::ArraySort | F::ArraySortIndices => {
                // v6 sorts int, float and string arrays (`sort_field` for
                // arrays of user-defined types is not implemented yet).
                if *elem_ty != Type::String {
                    self.require_numeric_array(func, elem_ty, args, span)?;
                }
                if rest.len() > 1 {
                    self.arity_error(func, "1 or 2", args, span);
                    return None;
                }
                let order = if let Some(arg) = rest.first() {
                    self.check_int_arg(func, "order", arg)?
                } else {
                    Self::const_int_expr(0, span)
                };
                final_args.push(order);
            }

            // percentrank: (index:int). Requires a numeric receiver.
            F::ArrayPercentRank => {
                self.require_numeric_array(func, elem_ty, args, span)?;
                let idx = self.check_int_arg(func, "index", &rest[0])?;
                final_args.push(idx);
            }

            // percentile family: (percentage:float). Requires a numeric
            // receiver; the percentage arg coerces int → float.
            F::ArrayPercentileNearestRank | F::ArrayPercentileLinearInterpolation => {
                self.require_numeric_array(func, elem_ty, args, span)?;
                let pct = self.check_float_arg(func, "percentage", &rest[0])?;
                final_args.push(pct);
            }

            // binary search variants: (val:elem). Requires a numeric receiver;
            // the value coerces int → float against the element type.
            F::ArrayBinarySearch | F::ArrayBinarySearchLeftmost | F::ArrayBinarySearchRightmost => {
                self.require_numeric_array(func, elem_ty, args, span)?;
                let v = self.analyze_expr_expecting(&rest[0].value, Some(elem_ty));
                let v = self.coerce_element_arg(func, elem_ty, v, rest[0].value.span())?;
                final_args.push(v);
            }

            // covariance: (other) or (other, biased). Both arrays must be
            // numeric; `biased` (bool) defaults to true. The CSIL call is always
            // ternary `(id1, id2, biased)`.
            F::ArrayCovariance => {
                self.require_numeric_array(func, elem_ty, args, span)?;
                if rest.is_empty() || rest.len() > 2 {
                    self.arity_error(func, "2 or 3", args, span);
                    return None;
                }
                let other = self.analyze_expr(&rest[0].value);
                if other.ty.is_error() {
                    return None;
                }
                match other.ty.array_elem() {
                    Some(Type::Int | Type::Float) => {}
                    _ => {
                        self.error(
                            codes::E0234,
                            format!(
                                "`array.covariance` requires two numeric arrays, found {} for the second argument",
                                other.ty.describe()
                            ),
                            rest[0].value.span(),
                            "the second argument must be a numeric array",
                        );
                        return None;
                    }
                }
                final_args.push(other);
                let biased = if let Some(arg) = rest.get(1) {
                    let b = self.analyze_expr_expecting(&arg.value, Some(&Type::Bool));
                    if !b.ty.is_error() && b.ty != Type::Bool {
                        self.error(
                            codes::E0234,
                            format!(
                                "`array.covariance` biased must be bool, found {}",
                                b.ty.describe()
                            ),
                            arg.value.span(),
                            "expected a bool",
                        );
                        return None;
                    }
                    b
                } else {
                    Self::const_bool_expr(true, span)
                };
                final_args.push(biased);
            }

            // Boolean reductions: require a bool or numeric receiver; no extra
            // arguments (v6 takes no callback).
            F::ArrayEvery | F::ArraySome => {
                self.require_bool_or_numeric_array(func, elem_ty, args, span)?;
            }

            _ => unreachable!("constructors handled elsewhere"),
        }
        Some((final_args, result))
    }

    /// Require a numeric (`int`/`float`) array receiver for the aggregate and
    /// ordering functions, reporting `E0235` otherwise. The receiver has
    /// already been confirmed to be an array (with element type `elem_ty`).
    pub(crate) fn require_numeric_array(
        &mut self,
        func: CollectionFunc,
        elem_ty: &Type,
        args: &[ast::CallArg],
        _span: Span,
    ) -> Option<()> {
        if matches!(elem_ty, Type::Int | Type::Float) {
            Some(())
        } else {
            self.error(
                codes::E0235,
                format!(
                    "`{}` requires a numeric array, found array<{}>",
                    func.name(),
                    elem_ty.describe()
                ),
                args[0].value.span(),
                "this operation only applies to int or float arrays",
            );
            None
        }
    }

    /// Require a bool or numeric (`int`/`float`) array receiver for the boolean
    /// reductions (`array.every`/`array.some`), reporting `E0235` otherwise. In
    /// v6 these apply to bool arrays (element = its own truth) and numeric
    /// arrays (element = non-zero, non-`na`).
    pub(crate) fn require_bool_or_numeric_array(
        &mut self,
        func: CollectionFunc,
        elem_ty: &Type,
        args: &[ast::CallArg],
        _span: Span,
    ) -> Option<()> {
        if matches!(elem_ty, Type::Bool | Type::Int | Type::Float) {
            Some(())
        } else {
            self.error(
                codes::E0235,
                format!(
                    "`{}` requires a bool or numeric array, found array<{}>",
                    func.name(),
                    elem_ty.describe()
                ),
                args[0].value.span(),
                "this operation only applies to bool, int, or float arrays",
            );
            None
        }
    }

    /// Analyze an argument that must be `int`, reporting `E0234` otherwise.
    pub(crate) fn check_int_arg(
        &mut self,
        func: CollectionFunc,
        param: &str,
        arg: &ast::CallArg,
    ) -> Option<HirExpr> {
        let hir = self.analyze_expr(&arg.value);
        if hir.ty.is_error() {
            return None;
        }
        if hir.ty != Type::Int {
            self.error(
                codes::E0234,
                format!(
                    "`{}` {param} must be int, found {}",
                    func.name(),
                    hir.ty.describe()
                ),
                arg.value.span(),
                "expected an int",
            );
            return None;
        }
        Some(hir)
    }

    /// Analyze an argument that must be `float` (int coerces via `IntToFloat`),
    /// reporting `E0234` otherwise.
    pub(crate) fn check_float_arg(
        &mut self,
        func: CollectionFunc,
        param: &str,
        arg: &ast::CallArg,
    ) -> Option<HirExpr> {
        let hir = self.analyze_expr_expecting(&arg.value, Some(&Type::Float));
        if hir.ty.is_error() {
            return None;
        }
        if hir.ty == Type::Float {
            return Some(hir);
        }
        if hir.ty == Type::Int {
            return Some(Self::coerce_int_to_float(hir));
        }
        self.error(
            codes::E0234,
            format!(
                "`{}` {param} must be float, found {}",
                func.name(),
                hir.ty.describe()
            ),
            arg.value.span(),
            "expected a float",
        );
        None
    }

    /// Coerce/validate an element argument against the array's element type.
    /// The only implicit coercion is int → float (v6 strictness); `na` adopts
    /// the element type. A mismatch is `E0234`.
    pub(crate) fn coerce_element_arg(
        &mut self,
        func: CollectionFunc,
        elem_ty: &Type,
        hir: HirExpr,
        span: Span,
    ) -> Option<HirExpr> {
        self.coerce_element_arg_named(func.name(), elem_ty, hir, span)
    }

    /// [`Self::coerce_element_arg`] with the callee spelled `name` in the
    /// diagnostic.
    pub(crate) fn coerce_element_arg_named(
        &mut self,
        name: &str,
        elem_ty: &Type,
        hir: HirExpr,
        span: Span,
    ) -> Option<HirExpr> {
        if hir.ty.is_error() {
            return None;
        }
        if matches!(hir.kind, HirExprKind::ConstNa) {
            return Some(HirExpr {
                ty: elem_ty.clone(),
                ..hir
            });
        }
        if hir.ty == *elem_ty {
            return Some(hir);
        }
        if *elem_ty == Type::Float && hir.ty == Type::Int {
            return Some(Self::coerce_int_to_float(hir));
        }
        self.error(
            codes::E0234,
            format!(
                "`{name}` element must be {}, found {}",
                elem_ty.describe(),
                hir.ty.describe()
            ),
            span,
            format!("expected {}", elem_ty.describe()),
        );
        None
    }

    /// True when both types are numeric (int/float) — an `array.from` pair
    /// that widens to float.
    pub(crate) fn numeric_pair(a: &Type, b: &Type) -> bool {
        matches!(a, Type::Int | Type::Float) && matches!(b, Type::Int | Type::Float)
    }

    /// A const int HIR expression (constructor `size` default).
    pub(crate) fn const_int_expr(value: i64, span: Span) -> HirExpr {
        HirExpr {
            kind: HirExprKind::ConstInt(value),
            ty: Type::Int,
            qualifier: Qualifier::Const,
            span,
        }
    }

    /// A const string HIR expression (materialized optional string default).
    pub(crate) fn const_string_expr(value: &str, span: Span) -> HirExpr {
        HirExpr {
            kind: HirExprKind::ConstString(value.to_string()),
            ty: Type::String,
            qualifier: Qualifier::Const,
            span,
        }
    }

    pub(crate) fn const_bool_expr(value: bool, span: Span) -> HirExpr {
        HirExpr {
            kind: HirExprKind::ConstBool(value),
            ty: Type::Bool,
            qualifier: Qualifier::Const,
            span,
        }
    }

    /// Emit the collection-call arity error (`E0204`).
    pub(crate) fn arity_error(
        &mut self,
        func: CollectionFunc,
        expected: &str,
        args: &[ast::CallArg],
        span: Span,
    ) {
        self.error(
            codes::E0204,
            format!(
                "`{}` expects {expected} argument(s), found {}",
                func.name(),
                args.len()
            ),
            span,
            format!("expected {expected} argument(s)"),
        );
        for a in args {
            let _ = self.analyze_expr(&a.value);
        }
    }

    /// `nz` result typing: the result takes the source's type; a given
    /// replacement must match it (mixed int/float widens both sides to
    /// float, `E0221` otherwise); an omitted replacement becomes the typed
    /// zero (`0`, `0.0`, transparent black).
    pub(crate) fn nz_arguments(
        &mut self,
        func: &BuiltinFunction,
        args: &[ast::Expr],
        mut checked: Vec<HirExpr>,
        span: Span,
    ) -> Option<(Vec<HirExpr>, Type)> {
        // A bare missing literal adopts the other operand's type. Arguments
        // have already been analyzed exactly once, preserving call-site state.
        if checked.len() == 2 {
            for i in 0..2 {
                if Self::na_literal_span(&args[i]).is_some()
                    && Self::na_literal_span(&args[1 - i]).is_none()
                {
                    checked[i].ty = checked[1 - i].ty.clone();
                }
            }
        }
        let src_ty = checked[0].ty.clone();
        if checked.len() == 1 {
            let default_kind = match src_ty {
                Type::Int => HirExprKind::ConstInt(0),
                Type::Float => HirExprKind::ConstFloat(0.0),
                Type::Color => HirExprKind::ConstColor {
                    r: 0,
                    g: 0,
                    b: 0,
                    t: 100.0,
                },
                _ => unreachable!("nz sources are int, float, or color (checked above)"),
            };
            checked.push(HirExpr {
                kind: default_kind,
                ty: src_ty.clone(),
                qualifier: Qualifier::Const,
                span,
            });
            return Some((checked, src_ty));
        }
        let rep_ty = checked[1].ty.clone();
        match (&src_ty, &rep_ty) {
            (a, b) if a == b => Some((checked, src_ty)),
            (Type::Float, Type::Int) => {
                let rep = checked.pop().expect("two args");
                checked.push(Self::coerce_int_to_float(rep));
                Some((checked, Type::Float))
            }
            (Type::Int, Type::Float) => {
                let src = checked.remove(0);
                checked.insert(0, Self::coerce_int_to_float(src));
                Some((checked, Type::Float))
            }
            _ => {
                self.error(
                    codes::E0221,
                    format!(
                        "`{}` replacement must match the source type ({}), found {}",
                        func.name,
                        src_ty.describe(),
                        rep_ty.describe()
                    ),
                    args[1].span(),
                    format!("expected {}", src_ty.describe()),
                );
                None
            }
        }
    }

    /// `E0222` for a series-materializing construct (`ta.*` call site or
    /// compound-history materialization) inside a loop body. Per-call-site
    /// state assumes once-per-bar execution; a loop would advance it multiple
    /// times per bar, so this is a strict compile error.
    /// Returns true when it reported an error.
    pub(crate) fn reject_stateful_in_loop(&mut self, what: &str, span: Span) -> bool {
        if self.loop_depth == 0 {
            return false;
        }
        self.error(
            codes::E0222,
            format!("{what} is not allowed inside a loop body"),
            span,
            "per-call-site state assumes once-per-bar execution",
        );
        true
    }

    /// `W0004` for series-materializing constructs (materialized history
    /// sites, `ta.*` call sites) that do not execute on every bar. Inside a
    /// `var` initializer the construct executes only once, so its hidden
    /// series records a single bar and reads over it are permanently `na`;
    /// inside a conditional branch its hidden series is stored only on bars
    /// where the branch executes, so history reads see `na` gaps.
    pub(crate) fn warn_if_not_executed_every_bar(&mut self, what: &str, storage: &str, span: Span) {
        if self.in_var_initializer {
            // The run-once case subsumes the conditional-branch case: a
            // `var` initializer inside a branch still executes only once.
            self.warning(
                codes::W0004,
                format!(
                    "{what} inside a `var` initializer executes only once; the series it records will be permanently `na` after the first bar"
                ),
                span,
                format!("{storage} records only the first bar it executes on"),
            );
        } else if self.in_conditional_branch() || self.ternary_branch_depth > 0 {
            self.warning(
                codes::W0004,
                format!(
                    "{what} inside a conditional branch should be executed on every bar for consistent results"
                ),
                span,
                format!("bars where this branch does not execute record `na` in {storage}"),
            );
        }
    }
}
