//! A model of a parsed v5 script: scopes, declarations, and the
//! type and qualifier of every expression.
//!
//! The compiler's checker targets Pine v6 syntax. This crate rewrites v5
//! constructs into v6 forms. This model is deliberately smaller. It resolves names, infers
//! types and qualifiers under v5's rules (including the ones v6 changed:
//! `int / int` stays `int`, a reassigned variable keeps its initializer's
//! qualifier), and records, per expression span, every type the expression
//! takes. User functions are generic in Pine, so a body is analyzed once per
//! distinct argument signature and each instantiation adds its types.
//! Rules then ask questions by span: "can this condition be numeric?",
//! "is this division `const int / const int` in every instantiation?".

use crate::builtins::{self, Effect, Qual, Ty};
use pinewasm_syntax as ast;
use pinewasm_syntax::{Expr, Stmt};
use std::collections::{HashMap, HashSet};

/// Identifies an expression by its source range and node kind (two nodes
/// can share a range only when they are different kinds).
pub(crate) type Key = (u32, u32, u8);

/// A function instantiation: its id with argument types and qualifiers.
type Instantiation = (String, Vec<Ty>, Vec<Qual>);

pub(crate) fn key(expr: &Expr) -> Key {
    let span = expr.span();
    let kind = match expr {
        Expr::Int { .. } => 0,
        Expr::Float { .. } => 1,
        Expr::Bool { .. } => 2,
        Expr::Na { .. } => 3,
        Expr::Str { .. } => 4,
        Expr::Color { .. } => 5,
        Expr::Ident(_) => 6,
        Expr::Unary { .. } => 7,
        Expr::Binary { .. } => 8,
        Expr::Ternary { .. } => 9,
        Expr::Call { .. } => 10,
        Expr::MethodCall { .. } => 11,
        Expr::FieldAccess { .. } => 12,
        Expr::History { .. } => 13,
        Expr::Group { .. } => 14,
        Expr::If(_) => 15,
        Expr::Switch(_) => 16,
        Expr::Tuple { .. } => 17,
        Expr::Loop(_) => 18,
    };
    (span.start, span.end, kind)
}

/// Strip grouping parentheses.
pub(crate) fn peel(expr: &Expr) -> &Expr {
    match expr {
        Expr::Group { inner, .. } => peel(inner),
        other => other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeclKind {
    Global,
    Local,
    Param,
    LoopVar,
}

/// One declared variable (or parameter), identified by the start offset of
/// its name.
#[derive(Debug, Clone)]
pub(crate) struct Decl {
    pub(crate) name: String,
    pub(crate) kind: DeclKind,
    pub(crate) is_var: bool,
    /// Declared type, when written (`bool b = na`).
    pub(crate) annotation: Option<Ty>,
    /// Every type the variable takes across instantiations.
    pub(crate) tys: Vec<Ty>,
    /// Qualifier of the initializer, which the model keeps after `:=`.
    pub(crate) init_qual: Qual,
    /// Whether any `:=` or compound assignment targets the variable.
    pub(crate) reassigned: bool,
    /// Offsets of the `:=` statements that target it.
    pub(crate) reassign_sites: Vec<u32>,
    /// Whether any reassignment sits in a block nested inside the
    /// declaration's own block (or in a function, for a global).
    pub(crate) nested_reassign: bool,
    /// Qualifiers of the reassigned values.
    pub(crate) reassign_quals: Vec<Qual>,
}

impl Decl {
    /// The qualifier this model gives the variable. It is the join of the
    /// initializer and every reassigned value, so a variable reassigned
    /// only with constants at its own block level stays `const`. A
    /// reassigned `var` is at least `simple`; any reassignment in a
    /// nested block makes it `series`.
    pub(crate) fn v5_qual(&self) -> Qual {
        if !self.reassigned {
            return self.init_qual;
        }
        if self.nested_reassign || self.kind == DeclKind::LoopVar {
            return Qual::Series;
        }
        let joined = self
            .reassign_quals
            .iter()
            .copied()
            .fold(self.init_qual, Qual::max);
        if self.is_var {
            joined.max(Qual::Simple)
        } else {
            joined
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ExprInfo {
    pub(crate) tys: Vec<Ty>,
    pub(crate) quals: Vec<Qual>,
}

#[derive(Debug, Clone)]
struct Binding {
    decl: Option<u32>,
    ty: Ty,
    qual: Qual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Global,
    Function,
    Block,
}

/// What the model learned about a user function.
#[derive(Debug, Clone, Default)]
pub(crate) struct FnFacts {
    /// Contains history, persistent state, or effects: skipping a call
    /// changes later results.
    pub(crate) must_evaluate: bool,
    /// Contains a `request.*()` call, directly or through callees.
    pub(crate) requests: bool,
}

pub(crate) struct Model<'p> {
    pub(crate) program: &'p ast::Program,
    pub(crate) funcs: HashMap<String, &'p ast::FuncDef>,
    methods: HashMap<String, Vec<&'p ast::FuncDef>>,
    pub(crate) udts: HashMap<String, &'p ast::TypeDecl>,
    enums: HashSet<String>,
    pub(crate) imports: HashSet<String>,
    pub(crate) decls: HashMap<u32, Decl>,
    pub(crate) exprs: HashMap<Key, ExprInfo>,
    /// Identifier reference (name start) → declaration.
    pub(crate) refs: HashMap<u32, u32>,
    /// Every type an `na` literal adopts from its context.
    pub(crate) na_adopted: HashMap<Key, Vec<Ty>>,
    /// Expressions that sit where v6 requires a `bool`.
    pub(crate) bool_context: HashSet<Key>,
    pub(crate) fn_facts: HashMap<String, FnFacts>,
    /// Declaration → its initializer expression.
    inits: HashMap<u32, &'p Expr>,
    /// Method-style calls of built-ins (`a.sort(...)`, `x.f().push(...)`),
    /// keyed by the start of the callee or method name, to the built-in
    /// they resolve to (`array.sort`).
    pub(crate) resolved_methods: HashMap<u32, String>,
    scopes: Vec<(Scope, HashMap<String, Binding>)>,
    memo: HashMap<Instantiation, (Ty, Qual)>,
    active: Vec<String>,
    instantiated: HashSet<String>,
    pub(crate) script_kind: ScriptKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScriptKind {
    Indicator,
    Strategy,
    Library,
    Unknown,
}

impl<'p> Model<'p> {
    pub(crate) fn build(program: &'p ast::Program) -> Self {
        let mut model = Model {
            program,
            funcs: HashMap::new(),
            methods: HashMap::new(),
            udts: HashMap::new(),
            enums: HashSet::new(),
            imports: HashSet::new(),
            decls: HashMap::new(),
            exprs: HashMap::new(),
            refs: HashMap::new(),
            na_adopted: HashMap::new(),
            bool_context: HashSet::new(),
            fn_facts: HashMap::new(),
            inits: HashMap::new(),
            resolved_methods: HashMap::new(),
            scopes: vec![(Scope::Global, HashMap::new())],
            memo: HashMap::new(),
            active: Vec::new(),
            instantiated: HashSet::new(),
            script_kind: ScriptKind::Unknown,
        };
        for item in &program.items {
            match item {
                ast::Item::FuncDef(f) if f.receiver.is_some() => {
                    model
                        .methods
                        .entry(f.name.name.clone())
                        .or_default()
                        .push(f);
                }
                ast::Item::FuncDef(f) => {
                    model.funcs.insert(f.name.name.clone(), f);
                }
                ast::Item::TypeDef(t) => {
                    model.udts.insert(t.name.name.clone(), t);
                }
                ast::Item::EnumDef(e) => {
                    model.enums.insert(e.name.name.clone());
                }
                ast::Item::Import(i) => {
                    model.imports.insert(i.alias.name.clone());
                }
                ast::Item::Indicator(_) => model.script_kind = ScriptKind::Indicator,
                ast::Item::Strategy(_) => model.script_kind = ScriptKind::Strategy,
                ast::Item::Library(_) => model.script_kind = ScriptKind::Library,
                ast::Item::Stmt(_) => {}
            }
        }
        model.compute_fn_facts();
        model.collect_reassignments();
        // A variable's qualifier joins its reassigned values' qualifiers,
        // which depend on other variables' qualifiers: type the script until
        // those joins stop changing, then keep only the final pass's facts.
        let mut previous: Vec<(u32, Qual)> = Vec::new();
        for _ in 0..4 {
            model.exprs.clear();
            model.na_adopted.clear();
            model.bool_context.clear();
            model.memo.clear();
            model.instantiated.clear();
            model.resolved_methods.clear();
            for decl in model.decls.values_mut() {
                decl.tys.clear();
            }
            model.type_program();
            let mut current: Vec<(u32, Qual)> = Vec::new();
            for (id, decl) in &mut model.decls {
                if let Some(max) = decl.reassign_quals.iter().copied().max() {
                    // Qualifiers only strengthen from pass to pass.
                    decl.reassign_quals = vec![max];
                    current.push((*id, max));
                }
            }
            current.sort();
            if current == previous {
                break;
            }
            previous = current;
        }
        model
    }

    fn type_program(&mut self) {
        let program = self.program;
        self.scopes = vec![(Scope::Global, HashMap::new())];
        for item in &program.items {
            match item {
                ast::Item::Stmt(stmt) => self.stmt(stmt),
                ast::Item::Indicator(d) => {
                    self.args(&d.args);
                }
                ast::Item::Strategy(d) => {
                    self.args(&d.args);
                }
                ast::Item::Library(d) => {
                    self.args(&d.args);
                }
                ast::Item::TypeDef(t) => {
                    for field in &t.fields {
                        if let Some(default) = &field.default {
                            let ty = field_type(&field.ty);
                            self.expr_expect(default, Some(&ty));
                        }
                    }
                }
                _ => {}
            }
        }
        // Functions never called from the script still get checked, with
        // their declared parameter types (or unknown ones).
        let uncalled: Vec<&ast::FuncDef> = program
            .items
            .iter()
            .filter_map(|item| match item {
                ast::Item::FuncDef(f) if !self.instantiated.contains(&fn_id(f)) => Some(f),
                _ => None,
            })
            .collect();
        for f in uncalled {
            let tys: Vec<Ty> = f
                .params
                .iter()
                .map(|p| p.ty.as_ref().map_or(Ty::Unknown, field_type))
                .collect();
            let quals = vec![Qual::Series; tys.len()];
            self.instantiate(f, &tys, &quals);
        }
    }

    // -- queries ---------------------------------------------------------------

    pub(crate) fn info(&self, expr: &Expr) -> Option<&ExprInfo> {
        self.exprs.get(&key(expr))
    }

    /// The expression's type, joined across instantiations.
    pub(crate) fn ty(&self, expr: &Expr) -> Ty {
        self.info(expr).map_or(Ty::Unknown, |info| {
            info.tys
                .iter()
                .fold(None::<Ty>, |acc, t| {
                    Some(acc.map_or_else(|| t.clone(), |a| a.join(t)))
                })
                .unwrap_or(Ty::Unknown)
        })
    }

    /// Whether any instantiation gives the expression a numeric type.
    pub(crate) fn may_be_numeric(&self, expr: &Expr) -> bool {
        self.info(expr)
            .is_some_and(|info| info.tys.iter().any(Ty::is_numeric))
    }

    /// Whether every instantiation types the expression as `const int`.
    pub(crate) fn always_const_int(&self, expr: &Expr) -> bool {
        self.info(expr).is_some_and(|info| {
            !info.tys.is_empty()
                && info.tys.iter().all(|t| *t == Ty::Int)
                && info.quals.iter().all(|q| *q == Qual::Const)
        })
    }

    /// Whether some instantiation types the expression as `const int`.
    pub(crate) fn sometimes_const_int(&self, expr: &Expr) -> bool {
        self.info(expr).is_some_and(|info| {
            info.tys
                .iter()
                .zip(&info.quals)
                .any(|(t, q)| *t == Ty::Int && *q == Qual::Const)
        })
    }

    pub(crate) fn max_qual(&self, expr: &Expr) -> Option<Qual> {
        self.info(expr)
            .and_then(|info| info.quals.iter().copied().max())
    }

    /// The initializer of the variable an identifier refers to.
    pub(crate) fn init_of(&self, ident: &ast::Ident) -> Option<&'p Expr> {
        self.refs
            .get(&ident.span.start)
            .and_then(|d| self.inits.get(d))
            .copied()
    }

    /// The declaration an identifier reference resolves to.
    pub(crate) fn decl_of(&self, ident: &ast::Ident) -> Option<&Decl> {
        self.refs
            .get(&ident.span.start)
            .and_then(|d| self.decls.get(d))
    }

    /// Whether evaluating `expr` has effects later bars can observe, so v6
    /// must evaluate it exactly where v5 did.
    pub(crate) fn must_evaluate(&self, expr: &Expr) -> bool {
        let mut found = false;
        self.walk_must_evaluate(expr, &mut found);
        found
    }

    fn walk_must_evaluate(&self, expr: &Expr, found: &mut bool) {
        if *found {
            return;
        }
        match expr {
            Expr::Call { callee, args, .. } => {
                if self.call_must_evaluate(&callee.name, args.first().map(|a| &a.value)) {
                    *found = true;
                    return;
                }
                for arg in args {
                    self.walk_must_evaluate(&arg.value, found);
                }
            }
            Expr::MethodCall {
                receiver,
                method,
                args,
                ..
            } => {
                let ty = self.ty(receiver);
                if self.method_must_evaluate(&ty, &method.name) {
                    *found = true;
                    return;
                }
                self.walk_must_evaluate(receiver, found);
                for arg in args {
                    self.walk_must_evaluate(&arg.value, found);
                }
            }
            Expr::History { target, offset, .. } => {
                // History of a named variable reads a buffer that exists no
                // matter where it is read. History of anything else creates
                // a buffer at this site, which only advances when evaluated.
                if !matches!(peel(target), Expr::Ident(_)) {
                    *found = true;
                    return;
                }
                self.walk_must_evaluate(offset, found);
            }
            Expr::If(i) => {
                self.walk_must_evaluate(&i.cond, found);
                for s in i.then_branch.iter().chain(i.else_branch.iter().flatten()) {
                    self.walk_stmt_must_evaluate(s, found);
                }
            }
            Expr::Switch(s) => {
                if let Some(subject) = &s.subject {
                    self.walk_must_evaluate(subject, found);
                }
                for arm in &s.arms {
                    if let Some(label) = &arm.label {
                        self.walk_must_evaluate(label, found);
                    }
                    for s in &arm.body {
                        self.walk_stmt_must_evaluate(s, found);
                    }
                }
            }
            Expr::Loop(stmt) => self.walk_stmt_must_evaluate(stmt, found),
            _ => for_each_child(expr, &mut |child| self.walk_must_evaluate(child, found)),
        }
    }

    fn walk_stmt_must_evaluate(&self, stmt: &Stmt, found: &mut bool) {
        if let Stmt::Assign { is_var: true, .. } = stmt {
            *found = true;
            return;
        }
        for_each_stmt_expr(stmt, &mut |e| self.walk_must_evaluate(e, found));
        for block in child_blocks(stmt) {
            for s in block {
                self.walk_stmt_must_evaluate(s, found);
            }
        }
    }

    fn call_must_evaluate(&self, name: &str, first_arg: Option<&Expr>) -> bool {
        if let Some(facts) = self.fn_facts.get(name) {
            return facts.must_evaluate;
        }
        if builtins::is_known_function(name) {
            return builtins::effect(name) != Effect::Pure;
        }
        if let Some((head, method)) = name.rsplit_once('.') {
            if self.udts.contains_key(head) && matches!(method, "new" | "copy") {
                return false;
            }
            if self
                .imports
                .contains(head.split('.').next().unwrap_or(head))
            {
                return true;
            }
            // `receiver.method(...)` written with a dotted callee.
            let receiver_ty = self.dotted_value_type(head);
            if receiver_ty != Ty::Unknown {
                return self.method_must_evaluate(&receiver_ty, method);
            }
        }
        let _ = first_arg;
        // Unknown callee: assume the worst.
        true
    }

    fn method_must_evaluate(&self, receiver: &Ty, method: &str) -> bool {
        if let Some(defs) = self.methods.get(method) {
            if defs.iter().any(|f| receiver_matches(f, receiver)) {
                return self
                    .fn_facts
                    .get(&method_id(method))
                    .is_none_or(|f| f.must_evaluate);
            }
        }
        match receiver.method_namespace() {
            Some(ns) => builtins::effect(&format!("{ns}.{method}")) != Effect::Pure,
            None => true,
        }
    }

    /// Type of a dotted name used as a value (variable field chains only),
    /// looked up in the final global scope; used outside the analysis walk.
    fn dotted_value_type(&self, name: &str) -> Ty {
        let mut segments = name.split('.');
        let Some(head) = segments.next() else {
            return Ty::Unknown;
        };
        // This fallback runs outside lexical scopes. Same-named declarations
        // can have incompatible types. Do not fold Ty::join over a HashMap:
        // Unknown is both its identity and conflict result, so that fold is
        // order-dependent. Only use a receiver type when every candidate
        // agrees; otherwise conservatively classify the call as effectful.
        let mut candidates = self
            .decls
            .values()
            .filter(|d| d.name == head)
            .flat_map(|d| d.tys.iter());
        let Some(first) = candidates.next() else {
            return Ty::Unknown;
        };
        if candidates.any(|ty| ty != first) {
            return Ty::Unknown;
        }
        let head_ty = first.clone();
        let mut ty = head_ty;
        for seg in segments {
            ty = self.field_ty(&ty, seg);
        }
        ty
    }

    fn field_ty(&self, ty: &Ty, field: &str) -> Ty {
        let Ty::Named(name) = ty else {
            return Ty::Unknown;
        };
        self.udts.get(name).map_or(Ty::Unknown, |t| {
            t.fields
                .iter()
                .find(|f| f.name.name == field)
                .map_or(Ty::Unknown, |f| field_type(&f.ty))
        })
    }

    // -- user-function facts ---------------------------------------------------

    fn compute_fn_facts(&mut self) {
        // Iterate to a fixpoint: a function is must-evaluate or requesting
        // when anything it calls is.
        let defs: Vec<(String, &'p ast::FuncDef)> = self
            .program
            .items
            .iter()
            .filter_map(|item| match item {
                ast::Item::FuncDef(f) => Some((fn_id(f), f)),
                _ => None,
            })
            .collect();
        for (id, _) in &defs {
            self.fn_facts.insert(id.clone(), FnFacts::default());
        }
        // Overloads (methods of different receivers, or library exports)
        // share an id; their facts are combined so the iteration converges.
        loop {
            let mut next: HashMap<String, FnFacts> = HashMap::new();
            for (id, def) in &defs {
                let mut facts = FnFacts::default();
                for stmt in &def.body {
                    self.stmt_facts(stmt, &mut facts);
                }
                let entry = next.entry(id.clone()).or_default();
                entry.must_evaluate |= facts.must_evaluate;
                entry.requests |= facts.requests;
            }
            let changed = next.iter().any(|(id, f)| {
                self.fn_facts.get(id).is_none_or(|old| {
                    old.must_evaluate != f.must_evaluate || old.requests != f.requests
                })
            });
            self.fn_facts = next;
            if !changed {
                break;
            }
        }
    }

    fn stmt_facts(&self, stmt: &Stmt, facts: &mut FnFacts) {
        if let Stmt::Assign { is_var: true, .. } = stmt {
            facts.must_evaluate = true;
        }
        for_each_stmt_expr(stmt, &mut |e| self.expr_facts(e, facts));
        for block in child_blocks(stmt) {
            for s in block {
                self.stmt_facts(s, facts);
            }
        }
    }

    fn expr_facts(&self, expr: &Expr, facts: &mut FnFacts) {
        match expr {
            Expr::History { .. } => facts.must_evaluate = true,
            Expr::Call { callee, .. } => {
                let name = callee.name.as_str();
                if name.starts_with("request.") {
                    facts.requests = true;
                    facts.must_evaluate = true;
                } else if let Some(f) = self.fn_facts.get(name) {
                    facts.must_evaluate |= f.must_evaluate;
                    facts.requests |= f.requests;
                } else if builtins::is_known_function(name) {
                    facts.must_evaluate |= builtins::effect(name) != Effect::Pure;
                } else if let Some((_, method)) = name.rsplit_once('.') {
                    if let Some(f) = self.fn_facts.get(&method_id(method)) {
                        facts.must_evaluate |= f.must_evaluate;
                        facts.requests |= f.requests;
                    } else {
                        let head = name.split('.').next().unwrap_or(name);
                        if self.imports.contains(head) {
                            facts.must_evaluate = true;
                        } else if !self.udts.contains_key(head) {
                            // A method of a built-in type: judge by its name
                            // across the collection and drawing namespaces.
                            facts.must_evaluate |= [
                                "array", "matrix", "map", "label", "line", "box", "table",
                                "linefill", "polyline",
                            ]
                            .iter()
                            .any(|ns| builtins::effect(&format!("{ns}.{method}")) != Effect::Pure);
                        }
                    }
                }
            }
            Expr::MethodCall { method, .. } => {
                if let Some(f) = self.fn_facts.get(&method_id(&method.name)) {
                    facts.must_evaluate |= f.must_evaluate;
                    facts.requests |= f.requests;
                } else {
                    facts.must_evaluate |= [
                        "array", "matrix", "map", "label", "line", "box", "table", "linefill",
                        "polyline",
                    ]
                    .iter()
                    .any(|ns| builtins::effect(&format!("{ns}.{}", method.name)) != Effect::Pure);
                }
            }
            _ => {}
        }
        match expr {
            Expr::If(i) => {
                self.expr_facts(&i.cond, facts);
                for s in i.then_branch.iter().chain(i.else_branch.iter().flatten()) {
                    self.stmt_facts(s, facts);
                }
            }
            Expr::Switch(s) => {
                if let Some(subject) = &s.subject {
                    self.expr_facts(subject, facts);
                }
                for arm in &s.arms {
                    if let Some(label) = &arm.label {
                        self.expr_facts(label, facts);
                    }
                    for s in &arm.body {
                        self.stmt_facts(s, facts);
                    }
                }
            }
            Expr::Loop(stmt) => self.stmt_facts(stmt, facts),
            _ => for_each_child(expr, &mut |child| self.expr_facts(child, facts)),
        }
    }

    // -- reassignment pre-pass ---------------------------------------------------

    /// Resolve every `:=` target before typing, because qualifier
    /// inference depends on whether a variable is ever reassigned.
    fn collect_reassignments(&mut self) {
        let mut scopes: Vec<HashMap<String, u32>> = vec![HashMap::new()];
        let program = self.program;
        for item in &program.items {
            match item {
                ast::Item::Stmt(s) => self.pre_stmt(s, &mut scopes, DeclKind::Global),
                ast::Item::FuncDef(f) => {
                    scopes.push(HashMap::new());
                    for p in &f.params {
                        self.declare_pre(
                            &p.name,
                            DeclKind::Param,
                            false,
                            p.ty.as_ref().map(field_type),
                            &mut scopes,
                        );
                    }
                    for s in &f.body {
                        self.pre_stmt(s, &mut scopes, DeclKind::Local);
                    }
                    scopes.pop();
                }
                _ => {}
            }
        }
    }

    fn declare_pre(
        &mut self,
        name: &ast::Ident,
        kind: DeclKind,
        is_var: bool,
        annotation: Option<Ty>,
        scopes: &mut [HashMap<String, u32>],
    ) {
        let id = name.span.start;
        self.decls.entry(id).or_insert_with(|| Decl {
            name: name.name.clone(),
            kind,
            is_var,
            annotation,
            tys: Vec::new(),
            init_qual: Qual::Series,
            reassigned: false,
            reassign_sites: Vec::new(),
            nested_reassign: false,
            reassign_quals: Vec::new(),
        });
        if let Some(scope) = scopes.last_mut() {
            scope.insert(name.name.clone(), id);
        }
    }

    fn pre_stmt(&mut self, stmt: &'p Stmt, scopes: &mut Vec<HashMap<String, u32>>, kind: DeclKind) {
        match stmt {
            Stmt::Assign {
                name,
                is_var,
                annotation,
                value,
                ..
            } => {
                let ann = annotation.as_ref().map(annotation_type);
                self.declare_pre(name, kind, *is_var, ann, scopes);
                self.inits.insert(name.span.start, value);
            }
            Stmt::Destructure { targets, .. } => {
                for t in targets {
                    self.declare_pre(t, kind, false, None, scopes);
                }
            }
            Stmt::Reassign { name, span, .. } => {
                let head = name.name.split('.').next().unwrap_or(&name.name);
                if !name.name.contains('.') {
                    let found = scopes
                        .iter()
                        .rev()
                        .enumerate()
                        .find_map(|(depth, s)| s.get(head).map(|id| (depth, *id)));
                    if let Some((depth, id)) = found {
                        if let Some(decl) = self.decls.get_mut(&id) {
                            decl.reassigned = true;
                            decl.reassign_sites.push(span.start);
                            if depth > 0 {
                                decl.nested_reassign = true;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        let nested = if kind == DeclKind::Global {
            DeclKind::Local
        } else {
            kind
        };
        let block = |this: &mut Self,
                     body: &'p [Stmt],
                     scopes: &mut Vec<HashMap<String, u32>>,
                     bind: &[&ast::Ident]| {
            scopes.push(HashMap::new());
            for b in bind {
                this.declare_pre(b, DeclKind::LoopVar, false, None, scopes);
            }
            for s in body {
                this.pre_stmt(s, scopes, nested);
            }
            scopes.pop();
        };
        match stmt {
            Stmt::If(i) => {
                block(self, &i.then_branch, scopes, &[]);
                if let Some(e) = &i.else_branch {
                    block(self, e, scopes, &[]);
                }
            }
            Stmt::Once(o) => block(self, &o.body, scopes, &[]),
            Stmt::For(f) => block(self, &f.body, scopes, &[&f.counter]),
            Stmt::ForIn(f) => {
                let mut bind = vec![&f.element];
                if let Some(i) = &f.index {
                    bind.push(i);
                }
                block(self, &f.body, scopes, &bind);
            }
            Stmt::While(w) => block(self, &w.body, scopes, &[]),
            Stmt::Switch(s) => {
                for arm in &s.arms {
                    block(self, &arm.body, scopes, &[]);
                }
            }
            _ => {}
        }
        // Expression-position blocks (`x = if ...`) declare locals too.
        for_each_stmt_expr(stmt, &mut |e| self.pre_expr_blocks(e, scopes, nested));
    }

    fn pre_expr_blocks(
        &mut self,
        expr: &'p Expr,
        scopes: &mut Vec<HashMap<String, u32>>,
        kind: DeclKind,
    ) {
        match expr {
            Expr::If(i) => {
                for body in std::iter::once(&i.then_branch).chain(i.else_branch.iter()) {
                    scopes.push(HashMap::new());
                    for s in body {
                        self.pre_stmt(s, scopes, kind);
                    }
                    scopes.pop();
                }
            }
            Expr::Switch(s) => {
                for arm in &s.arms {
                    scopes.push(HashMap::new());
                    for s in &arm.body {
                        self.pre_stmt(s, scopes, kind);
                    }
                    scopes.pop();
                }
            }
            Expr::Loop(stmt) => self.pre_stmt(stmt, scopes, kind),
            _ => for_each_child(expr, &mut |c| self.pre_expr_blocks(c, scopes, kind)),
        }
    }

    // -- typing walk -------------------------------------------------------------

    fn record(&mut self, expr: &Expr, ty: &Ty, qual: Qual) {
        let info = self.exprs.entry(key(expr)).or_default();
        if !info
            .tys
            .iter()
            .zip(&info.quals)
            .any(|(t, q)| t == ty && *q == qual)
        {
            info.tys.push(ty.clone());
            info.quals.push(qual);
        }
    }

    fn adopt_na(&mut self, expr: &Expr, ty: &Ty) {
        let expr = peel(expr);
        if matches!(expr, Expr::Na { .. }) && !matches!(ty, Ty::Na | Ty::Unknown) {
            let entry = self.na_adopted.entry(key(expr)).or_default();
            if !entry.contains(ty) {
                entry.push(ty.clone());
            }
        }
    }

    fn mark_bool_context(&mut self, expr: &Expr) {
        // A conditional expression is converted branch by branch: its
        // values are what must be `bool`.
        match peel(expr) {
            Expr::If(i) => {
                for body in std::iter::once(&i.then_branch).chain(i.else_branch.iter()) {
                    self.mark_tail_bool(body);
                }
            }
            Expr::Switch(s) => {
                for arm in &s.arms {
                    self.mark_tail_bool(&arm.body);
                }
            }
            Expr::Loop(stmt) => {
                if let Some(body) = loop_body(stmt) {
                    self.mark_tail_bool(body);
                }
            }
            _ => {
                self.bool_context.insert(key(expr));
            }
        }
    }

    fn mark_tail_bool(&mut self, body: &[Stmt]) {
        match body.last() {
            Some(Stmt::Expr { expr, .. }) => self.mark_bool_context(expr),
            Some(Stmt::If(i)) => {
                self.mark_tail_bool(&i.then_branch);
                if let Some(e) = &i.else_branch {
                    self.mark_tail_bool(e);
                }
            }
            _ => {}
        }
    }

    fn lookup(&self, name: &str) -> Option<&Binding> {
        // Inside a function body only its own scopes and globals are visible.
        let mut crossed_function = false;
        for (kind, scope) in self.scopes.iter().rev() {
            if crossed_function && *kind != Scope::Global {
                continue;
            }
            if let Some(b) = scope.get(name) {
                return Some(b);
            }
            if *kind == Scope::Function {
                crossed_function = true;
            }
        }
        None
    }

    fn declare(&mut self, name: &ast::Ident, ty: Ty, qual: Qual) {
        let id = name.span.start;
        if let Some(decl) = self.decls.get_mut(&id) {
            if !decl.tys.contains(&ty) {
                decl.tys.push(ty.clone());
            }
            decl.init_qual = qual;
        }
        let decl = self.decls.contains_key(&id).then_some(id);
        if let Some((_, scope)) = self.scopes.last_mut() {
            scope.insert(name.name.clone(), Binding { decl, ty, qual });
        }
    }

    fn with_block(&mut self, body: &[Stmt]) {
        self.scopes.push((Scope::Block, HashMap::new()));
        for s in body {
            self.stmt(s);
        }
        self.scopes.pop();
    }

    /// Analyze a block whose last statement supplies a value.
    fn value_block(&mut self, body: &[Stmt]) -> Option<(Ty, Qual)> {
        self.scopes.push((Scope::Block, HashMap::new()));
        let mut result = None;
        for (i, s) in body.iter().enumerate() {
            if i + 1 == body.len() {
                result = self.stmt_value(s);
            } else {
                self.stmt(s);
            }
        }
        self.scopes.pop();
        result
    }

    /// Analyze a statement that may supply a block's value.
    fn stmt_value(&mut self, stmt: &Stmt) -> Option<(Ty, Qual)> {
        match stmt {
            Stmt::Expr { expr, .. } => Some(self.expr(expr)),
            Stmt::If(i) => {
                let (_, cq) = self.expr_expect(&i.cond, Some(&Ty::Bool));
                self.mark_bool_context(&i.cond);
                let then = self.value_block(&i.then_branch);
                let other = i.else_branch.as_ref().and_then(|b| self.value_block(b));
                let ty = join_opt(then.as_ref(), other.as_ref());
                Some((ty, cq.max(Qual::Series)))
            }
            Stmt::Switch(s) => Some(self.switch(s)),
            Stmt::Assign { .. } | Stmt::Reassign { .. } => {
                self.stmt(stmt);
                None
            }
            _ => {
                self.stmt(stmt);
                None
            }
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Assign {
                name,
                value,
                annotation,
                ..
            } => {
                let ann = annotation.as_ref().map(annotation_type);
                let (vty, vq) = self.expr_expect(value, ann.as_ref());
                if ann == Some(Ty::Bool) {
                    self.mark_bool_context(value);
                }
                let ty = ann.unwrap_or(match vty {
                    Ty::Na => Ty::Unknown,
                    t => t,
                });
                self.declare(name, ty, vq);
            }
            Stmt::Reassign { name, value, .. } => {
                let target = self.target_type(name);
                if target == Ty::Bool {
                    self.mark_bool_context(value);
                }
                let (_, q) = self.expr_expect(value, Some(&target));
                if let Some(d) = self.refs.get(&name.span.start).copied() {
                    if let Some(decl) = self.decls.get_mut(&d) {
                        if !decl.reassign_quals.contains(&q) {
                            decl.reassign_quals.push(q);
                        }
                    }
                }
            }
            Stmt::Destructure { targets, value, .. } => {
                let (ty, q) = self.expr(value);
                for (i, t) in targets.iter().enumerate() {
                    let elem = match &ty {
                        Ty::Tuple(items) => items.get(i).cloned().unwrap_or(Ty::Unknown),
                        _ => Ty::Unknown,
                    };
                    self.declare(t, elem, q);
                }
            }
            Stmt::Expr { expr, .. } => {
                self.expr(expr);
            }
            Stmt::If(i) => {
                self.expr_expect(&i.cond, Some(&Ty::Bool));
                self.mark_bool_context(&i.cond);
                self.with_block(&i.then_branch);
                if let Some(e) = &i.else_branch {
                    self.with_block(e);
                }
            }
            Stmt::Once(o) => {
                if let Some(cond) = &o.cond {
                    self.expr_expect(cond, Some(&Ty::Bool));
                    self.mark_bool_context(cond);
                }
                self.with_block(&o.body);
            }
            Stmt::For(f) => {
                self.expr(&f.start);
                self.expr(&f.end);
                if let Some(step) = &f.step {
                    self.expr(step);
                }
                self.scopes.push((Scope::Block, HashMap::new()));
                self.declare(&f.counter, Ty::Int, Qual::Series);
                for s in &f.body {
                    self.stmt(s);
                }
                self.scopes.pop();
            }
            Stmt::ForIn(f) => {
                let (ty, _) = self.expr(&f.iterable);
                self.scopes.push((Scope::Block, HashMap::new()));
                if let Some(i) = &f.index {
                    self.declare(i, Ty::Int, Qual::Series);
                }
                self.declare(&f.element, ty.element(), Qual::Series);
                for s in &f.body {
                    self.stmt(s);
                }
                self.scopes.pop();
            }
            Stmt::While(w) => {
                self.expr_expect(&w.cond, Some(&Ty::Bool));
                self.mark_bool_context(&w.cond);
                self.with_block(&w.body);
            }
            Stmt::Switch(s) => {
                self.switch(s);
            }
            Stmt::Break { .. } | Stmt::Continue { .. } => {}
        }
    }

    fn target_type(&mut self, name: &ast::Ident) -> Ty {
        if let Some((head, rest)) = name.name.split_once('.') {
            let mut ty = self.lookup(head).map_or(Ty::Unknown, |b| b.ty.clone());
            for seg in rest.split('.') {
                ty = self.field_ty(&ty, seg);
            }
            return ty;
        }
        if let Some(b) = self.lookup(&name.name) {
            let (decl, ty) = (b.decl, b.ty.clone());
            if let Some(d) = decl {
                self.refs.insert(name.span.start, d);
                if let Some(ann) = self.decls.get(&d).and_then(|d| d.annotation.clone()) {
                    return ann;
                }
            }
            return ty;
        }
        Ty::Unknown
    }

    fn switch(&mut self, s: &ast::SwitchStmt) -> (Ty, Qual) {
        let subject = s.subject.as_ref().map(|e| self.expr(e));
        let mut values: Vec<(Option<&Expr>, Ty)> = Vec::new();
        let mut qual = subject.as_ref().map_or(Qual::Const, |(_, q)| *q);
        let mut has_default = false;
        for arm in &s.arms {
            if let Some(label) = &arm.label {
                let (_, q) = if subject.is_some() {
                    self.expr(label)
                } else {
                    self.mark_bool_context(label);
                    self.expr_expect(label, Some(&Ty::Bool))
                };
                qual = qual.max(q);
            } else {
                has_default = true;
            }
            if let Some((ty, q)) = self.value_block(&arm.body) {
                qual = qual.max(q);
                let tail = match arm.body.last() {
                    Some(Stmt::Expr { expr, .. }) => Some(expr),
                    _ => None,
                };
                values.push((tail, ty));
            }
        }
        let mut ty = values.iter().fold(Ty::Na, |a, (_, t)| a.join(t));
        if !has_default && ty == Ty::Na {
            ty = Ty::Unknown;
        }
        for (tail, _) in &values {
            if let Some(e) = tail {
                self.adopt_na(e, &ty);
                if ty == Ty::Bool {
                    self.mark_bool_context(e);
                }
            }
        }
        (ty, qual.max(Qual::Series))
    }

    fn loop_expr(&mut self, stmt: &Stmt) -> (Ty, Qual) {
        let result = match stmt {
            Stmt::For(f) => {
                self.expr(&f.start);
                self.expr(&f.end);
                if let Some(step) = &f.step {
                    self.expr(step);
                }
                self.scopes.push((Scope::Block, HashMap::new()));
                self.declare(&f.counter, Ty::Int, Qual::Series);
                let result = self.value_block(&f.body);
                self.scopes.pop();
                result
            }
            Stmt::ForIn(f) => {
                let (iterable_ty, _) = self.expr(&f.iterable);
                self.scopes.push((Scope::Block, HashMap::new()));
                if let Some(index) = &f.index {
                    self.declare(index, Ty::Int, Qual::Series);
                }
                self.declare(&f.element, iterable_ty.element(), Qual::Series);
                let result = self.value_block(&f.body);
                self.scopes.pop();
                result
            }
            Stmt::While(w) => {
                let (_, cond_qual) = self.expr_expect(&w.cond, Some(&Ty::Bool));
                self.mark_bool_context(&w.cond);
                self.value_block(&w.body)
                    .map(|(ty, qual)| (ty, qual.max(cond_qual)))
            }
            _ => None,
        };
        let Some((ty, qual)) = result else {
            return (Ty::Unknown, Qual::Series);
        };
        if ty == Ty::Bool {
            if let Some(body) = loop_body(stmt) {
                self.mark_tail_bool(body);
            }
        }
        (ty, qual.max(Qual::Series))
    }

    fn args(&mut self, args: &[ast::CallArg]) -> (Vec<Ty>, Vec<Qual>) {
        let mut tys = Vec::with_capacity(args.len());
        let mut quals = Vec::with_capacity(args.len());
        for a in args {
            let (t, q) = self.expr(&a.value);
            tys.push(t);
            quals.push(q);
        }
        (tys, quals)
    }

    pub(crate) fn expr(&mut self, expr: &Expr) -> (Ty, Qual) {
        self.expr_expect(expr, None)
    }

    fn expr_expect(&mut self, expr: &Expr, expected: Option<&Ty>) -> (Ty, Qual) {
        let (ty, qual) = self.expr_inner(expr, expected);
        if let Some(t) = expected {
            self.adopt_na(expr, t);
        }
        self.record(expr, &ty, qual);
        (ty, qual)
    }

    #[allow(clippy::too_many_lines)]
    fn expr_inner(&mut self, expr: &Expr, expected: Option<&Ty>) -> (Ty, Qual) {
        match expr {
            Expr::Int { .. } => (Ty::Int, Qual::Const),
            Expr::Float { .. } => (Ty::Float, Qual::Const),
            Expr::Bool { .. } => (Ty::Bool, Qual::Const),
            Expr::Na { .. } => (Ty::Na, Qual::Const),
            Expr::Str { .. } => (Ty::Str, Qual::Const),
            Expr::Color { .. } => (Ty::Color, Qual::Const),
            Expr::Ident(ident) => self.ident(ident),
            Expr::Unary { op, operand, .. } => {
                let (t, q) = self.expr(operand);
                match op {
                    ast::UnaryOp::Not => {
                        self.mark_bool_context(operand);
                        (Ty::Bool, q)
                    }
                    _ => (t, q),
                }
            }
            Expr::Binary { op, lhs, rhs, .. } => {
                use ast::BinaryOp as B;
                let (lt, lq) = self.expr(lhs);
                let (rt, rq) = self.expr(rhs);
                let q = lq.max(rq);
                let ty = match op {
                    B::And | B::Or => {
                        self.mark_bool_context(lhs);
                        self.mark_bool_context(rhs);
                        Ty::Bool
                    }
                    B::Lt | B::Le | B::Gt | B::Ge | B::Eq | B::Ne => {
                        // Equality between a bool and a numeric operand treats
                        // the numeric operand as bool. Migration writes the
                        // cast on that operand explicitly.
                        if matches!(op, B::Eq | B::Ne) {
                            if lt == Ty::Bool && rt.is_numeric() {
                                self.mark_bool_context(rhs);
                            } else if rt == Ty::Bool && lt.is_numeric() {
                                self.mark_bool_context(lhs);
                            }
                        }
                        if matches!(peel(lhs), Expr::Na { .. }) {
                            self.adopt_na(lhs, &rt);
                        }
                        if matches!(peel(rhs), Expr::Na { .. }) {
                            self.adopt_na(rhs, &lt);
                        }
                        Ty::Bool
                    }
                    B::Add if lt == Ty::Str || rt == Ty::Str => Ty::Str,
                    _ => match (&lt, &rt) {
                        (Ty::Int, Ty::Int) => Ty::Int,
                        (Ty::Int | Ty::Float | Ty::Na, Ty::Int | Ty::Float | Ty::Na) => {
                            if lt == Ty::Na && rt == Ty::Na {
                                Ty::Float
                            } else if lt == Ty::Int || rt == Ty::Int {
                                if lt == Ty::Float || rt == Ty::Float {
                                    Ty::Float
                                } else {
                                    Ty::Int
                                }
                            } else {
                                Ty::Float
                            }
                        }
                        (Ty::Unknown, t) | (t, Ty::Unknown) if t.is_numeric() => {
                            if *t == Ty::Float {
                                Ty::Float
                            } else {
                                Ty::Unknown
                            }
                        }
                        _ => Ty::Unknown,
                    },
                };
                (ty, q)
            }
            Expr::Ternary {
                cond,
                then_e,
                else_e,
                ..
            } => {
                let (_, cq) = self.expr_expect(cond, Some(&Ty::Bool));
                self.mark_bool_context(cond);
                let (tt, tq) = self.expr_expect(then_e, expected);
                let (et, eq) = self.expr_expect(else_e, expected);
                let ty = tt.join(&et);
                self.adopt_na(then_e, &ty);
                self.adopt_na(else_e, &ty);
                if ty == Ty::Bool {
                    // A numeric branch of a boolean conditional is cast.
                    self.mark_bool_context(then_e);
                    self.mark_bool_context(else_e);
                }
                (ty, cq.max(tq).max(eq))
            }
            Expr::Call {
                callee,
                args,
                type_args,
                ..
            } => self.call(callee, args, type_args),
            Expr::MethodCall {
                receiver,
                method,
                args,
                ..
            } => {
                let (rty, rq) = self.expr(receiver);
                self.method_call(&rty, rq, &method.name, args, method.span.start)
            }
            Expr::FieldAccess {
                receiver, field, ..
            } => {
                let (receiver_ty, _) = self.expr(receiver);
                (self.field_ty(&receiver_ty, &field.name), Qual::Series)
            }
            Expr::History { target, offset, .. } => {
                let (t, _) = self.expr(target);
                self.expr(offset);
                (t, Qual::Series)
            }
            Expr::Group { inner, .. } => self.expr_expect(inner, expected),
            Expr::If(i) => {
                let (_, cq) = self.expr_expect(&i.cond, Some(&Ty::Bool));
                self.mark_bool_context(&i.cond);
                let then = self.value_block(&i.then_branch);
                let other = i.else_branch.as_ref().and_then(|b| self.value_block(b));
                let ty = join_opt(then.as_ref(), other.as_ref());
                for body in std::iter::once(&i.then_branch).chain(i.else_branch.iter()) {
                    if let Some(Stmt::Expr { expr, .. }) = body.last() {
                        self.adopt_na(expr, &ty);
                        if ty == Ty::Bool {
                            self.mark_bool_context(expr);
                        }
                    }
                }
                (ty, cq.max(Qual::Series))
            }
            Expr::Switch(s) => self.switch(s),
            Expr::Loop(stmt) => self.loop_expr(stmt),
            Expr::Tuple { elements, .. } => {
                let mut tys = Vec::new();
                let mut q = Qual::Const;
                for e in elements {
                    let (t, eq) = self.expr(e);
                    tys.push(t);
                    q = q.max(eq);
                }
                (Ty::Tuple(tys), q)
            }
        }
    }

    fn ident(&mut self, ident: &ast::Ident) -> (Ty, Qual) {
        let name = ident.name.as_str();
        if let Some(b) = self.lookup(name) {
            let (decl, ty, qual) = (b.decl, b.ty.clone(), b.qual);
            if let Some(d) = decl {
                self.refs.insert(ident.span.start, d);
                // Qualifier rule of this model: a variable is as constant as
                // its initializer. A reassigned variable is reported as `series`,
                // and the detector for reassigned variables that must be
                // constant handles the difference.
                if let Some(decl) = self.decls.get(&d) {
                    if decl.reassigned || decl.kind == DeclKind::LoopVar {
                        return (ty, decl.v5_qual());
                    }
                }
            }
            return (ty, qual);
        }
        if let Some((t, q)) = builtins::variable(name) {
            return (t, q);
        }
        if let Some(t) = builtins::constant(name) {
            return (t, Qual::Const);
        }
        if let Some((head, rest)) = name.split_once('.') {
            if self.enums.contains(head) {
                return (Ty::Named(head.to_string()), Qual::Const);
            }
            if let Some(b) = self.lookup(head) {
                let (decl, mut ty) = (b.decl, b.ty.clone());
                if let Some(d) = decl {
                    self.refs.insert(ident.span.start, d);
                }
                for seg in rest.split('.') {
                    ty = self.field_ty(&ty, seg);
                }
                return (ty, Qual::Series);
            }
        }
        (Ty::Unknown, Qual::Series)
    }

    fn call(
        &mut self,
        callee: &ast::Ident,
        args: &[ast::CallArg],
        type_args: &[ast::Ident],
    ) -> (Ty, Qual) {
        let name = callee.name.as_str();
        // User-defined function.
        if let Some(def) = self.funcs.get(name).copied() {
            let (tys, quals) = self.user_args(def, args);
            return self.instantiate(def, &tys, &quals);
        }
        // Generic constructors carry their element types structurally.
        if !type_args.is_empty() {
            self.args(args);
            let t = |i: usize| {
                type_args
                    .get(i)
                    .map_or(Ty::Unknown, |a| named_type(&a.name))
            };
            let ty = match name {
                "array.new" => Ty::Array(Box::new(t(0))),
                "matrix.new" => Ty::Matrix(Box::new(t(0))),
                "map.new" => Ty::Map(Box::new(t(0)), Box::new(t(1))),
                _ => Ty::Unknown,
            };
            return (ty, Qual::Series);
        }
        if let Some(rest) = name.strip_prefix("map.new_") {
            self.args(args);
            let mut parts = rest.splitn(2, '_');
            let k = parts.next().map_or(Ty::Unknown, named_type);
            let v = parts.next().map_or(Ty::Unknown, named_type);
            return (Ty::Map(Box::new(k), Box::new(v)), Qual::Series);
        }
        if builtins::is_known_function(name) && !self.is_value_head(name) {
            return self.builtin_call(name, args, None);
        }
        if let Some((head, method)) = name.rsplit_once('.') {
            // `Type.new(...)` / `Type.copy(x)` for user-defined types.
            if let Some(udt) = self.udts.get(head).copied() {
                if method == "new" {
                    for a in args {
                        let expected = a.name.as_ref().and_then(|n| {
                            udt.fields
                                .iter()
                                .find(|f| f.name.name == n.name)
                                .map(|f| field_type(&f.ty))
                        });
                        let (_, _) = self.expr_expect(&a.value, expected.as_ref());
                        if expected == Some(Ty::Bool) {
                            self.mark_bool_context(&a.value);
                        }
                    }
                    for (i, a) in args.iter().enumerate() {
                        if a.name.is_none() {
                            if let Some(f) = udt.fields.get(i) {
                                let ft = field_type(&f.ty);
                                self.adopt_na(&a.value, &ft);
                                if ft == Ty::Bool {
                                    self.mark_bool_context(&a.value);
                                }
                            }
                        }
                    }
                } else {
                    self.args(args);
                }
                return (Ty::Named(head.to_string()), Qual::Series);
            }
            let root = head.split('.').next().unwrap_or(head);
            if self.imports.contains(root) {
                self.args(args);
                return (Ty::Unknown, Qual::Series);
            }
            // `receiver.method(...)` where the receiver is a variable or a
            // field chain.
            if self.lookup(root).is_some() {
                let (rty, _) = self.ident(&ast::Ident {
                    name: head.to_string(),
                    span: callee.span,
                });
                return self.method_call(&rty, Qual::Series, method, args, callee.span.start);
            }
        }
        self.args(args);
        (Ty::Unknown, Qual::Series)
    }

    /// Whether a dotted callee's head is a user variable (so `x.get()` is a
    /// method call even when `x.get` looks like no built-in).
    fn is_value_head(&self, name: &str) -> bool {
        name.split_once('.')
            .is_some_and(|(head, _)| self.lookup(head).is_some())
    }

    fn user_args(&mut self, def: &ast::FuncDef, args: &[ast::CallArg]) -> (Vec<Ty>, Vec<Qual>) {
        let mut tys = vec![None; def.params.len()];
        let mut quals = vec![Qual::Const; def.params.len()];
        let mut position = 0usize;
        for a in args {
            let index = match &a.name {
                Some(n) => def.params.iter().position(|p| p.name.name == n.name),
                None => {
                    let i = position;
                    position += 1;
                    Some(i)
                }
            };
            let declared = index
                .and_then(|i| def.params.get(i))
                .and_then(|p| p.ty.as_ref().map(field_type));
            let (t, q) = self.expr_expect(&a.value, declared.as_ref());
            if declared == Some(Ty::Bool) {
                self.mark_bool_context(&a.value);
            }
            if let Some(i) = index.filter(|i| *i < tys.len()) {
                tys[i] = Some(declared.unwrap_or(t));
                quals[i] = q;
            }
        }
        let tys = def
            .params
            .iter()
            .zip(tys)
            .map(|(p, t)| {
                t.or_else(|| p.ty.as_ref().map(field_type))
                    .or_else(|| p.default.as_ref().map(literal_type))
                    .unwrap_or(Ty::Unknown)
            })
            .collect();
        (tys, quals)
    }

    fn instantiate(&mut self, def: &ast::FuncDef, tys: &[Ty], quals: &[Qual]) -> (Ty, Qual) {
        let id = fn_id(def);
        self.instantiated.insert(id.clone());
        let memo_key = (id.clone(), tys.to_vec(), quals.to_vec());
        if let Some(r) = self.memo.get(&memo_key) {
            return r.clone();
        }
        if self.active.contains(&id) || self.active.len() > 32 {
            return (Ty::Unknown, Qual::Series);
        }
        self.active.push(id);
        // A function sees only globals: park the local scopes.
        let parked: Vec<_> = self.scopes.drain(1..).collect();
        self.scopes.push((Scope::Function, HashMap::new()));
        for (i, p) in def.params.iter().enumerate() {
            if let Some(default) = &p.default {
                let declared = p.ty.as_ref().map(field_type);
                self.expr_expect(default, declared.as_ref());
            }
            let ty = tys.get(i).cloned().unwrap_or(Ty::Unknown);
            // An unqualified typed parameter is at least `simple` even when
            // every call passes a const argument. An untyped parameter can
            // inherit `const`; an explicit qualifier overrides inference.
            // The model types an untyped parameter by its qualifier, and
            // integer division in the body uses that type.
            let q = quals.get(i).copied().unwrap_or(Qual::Series);
            let q = match p.qualifier {
                Some(ast::ParamQualifier::Const) => Qual::Const,
                Some(ast::ParamQualifier::Input) => Qual::Input,
                Some(ast::ParamQualifier::Simple) => Qual::Simple,
                Some(ast::ParamQualifier::Series) => Qual::Series,
                None if p.ty.is_some() => q.max(Qual::Simple),
                None => q,
            };
            self.declare(&p.name, ty, q);
        }
        let mut result = (Ty::Void, Qual::Series);
        for (i, s) in def.body.iter().enumerate() {
            if i + 1 == def.body.len() {
                result = match s {
                    Stmt::Assign { value, .. } => {
                        self.stmt(s);
                        let t = self.ty(value);
                        (t, self.max_qual(value).unwrap_or(Qual::Series))
                    }
                    _ => self.stmt_value(s).unwrap_or((Ty::Void, Qual::Series)),
                };
            } else {
                self.stmt(s);
            }
        }
        self.scopes.truncate(1);
        self.scopes.extend(parked);
        self.active.pop();
        // Anything stateful inside makes the call's result a series.
        let facts_series = self
            .fn_facts
            .get(&fn_id(def))
            .is_some_and(|f| f.must_evaluate);
        if facts_series {
            result.1 = Qual::Series;
        }
        self.memo.insert(memo_key, result.clone());
        result
    }

    fn method_call(
        &mut self,
        receiver: &Ty,
        receiver_qual: Qual,
        method: &str,
        args: &[ast::CallArg],
        site: u32,
    ) -> (Ty, Qual) {
        if let Some(defs) = self.methods.get(method).cloned() {
            if let Some(def) = defs.iter().find(|f| receiver_matches(f, receiver)).copied() {
                let (mut tys, mut quals) = self.user_args_skip_receiver(def, args);
                tys.insert(0, receiver.clone());
                quals.insert(0, receiver_qual);
                return self.instantiate(def, &tys, &quals);
            }
        }
        match receiver.method_namespace() {
            Some(ns) => {
                let name = format!("{ns}.{method}");
                self.resolved_methods.insert(site, name.clone());
                self.builtin_call(&name, args, Some((receiver.clone(), receiver_qual)))
            }
            None => {
                self.args(args);
                (Ty::Unknown, Qual::Series)
            }
        }
    }

    fn user_args_skip_receiver(
        &mut self,
        def: &ast::FuncDef,
        args: &[ast::CallArg],
    ) -> (Vec<Ty>, Vec<Qual>) {
        let rest: Vec<&ast::FuncParam> = def.params.iter().skip(1).collect();
        let mut tys = vec![None; rest.len()];
        let mut quals = vec![Qual::Const; rest.len()];
        let mut position = 0usize;
        for a in args {
            let index = match &a.name {
                Some(n) => rest.iter().position(|p| p.name.name == n.name),
                None => {
                    let i = position;
                    position += 1;
                    Some(i)
                }
            };
            let declared = index
                .and_then(|i| rest.get(i))
                .and_then(|p| p.ty.as_ref().map(field_type));
            let (t, q) = self.expr_expect(&a.value, declared.as_ref());
            if let Some(i) = index.filter(|i| *i < tys.len()) {
                tys[i] = Some(declared.unwrap_or(t));
                quals[i] = q;
            }
        }
        let tys = rest
            .iter()
            .zip(tys)
            .map(|(p, t)| {
                t.or_else(|| p.ty.as_ref().map(field_type))
                    .unwrap_or(Ty::Unknown)
            })
            .collect();
        (tys, quals)
    }

    fn builtin_call(
        &mut self,
        name: &str,
        args: &[ast::CallArg],
        receiver: Option<(Ty, Qual)>,
    ) -> (Ty, Qual) {
        let mut tys = Vec::with_capacity(args.len() + 1);
        let mut quals = Vec::with_capacity(args.len() + 1);
        if let Some((t, q)) = &receiver {
            tys.push(t.clone());
            quals.push(*q);
        }
        // Element-typed arguments of collection mutators adopt the element
        // type (`array.push(flags, na)` on an `array<bool>`).
        let receiver_elem = receiver.as_ref().map(|(t, _)| t.element()).or_else(|| {
            args.first()
                .filter(|_| {
                    name.starts_with("array.")
                        || name.starts_with("map.")
                        || name.starts_with("matrix.")
                })
                .map(|a| {
                    let (t, _) = self.expr(&a.value);
                    t.element()
                })
        });
        let positions = builtins::bool_param_positions(name);
        let offset = usize::from(receiver.is_some());
        for (i, a) in args.iter().enumerate() {
            if receiver.is_none() && i == 0 && receiver_elem.is_some() {
                // Already typed above.
                let t = self.ty(&a.value);
                tys.push(t);
                quals.push(self.max_qual(&a.value).unwrap_or(Qual::Series));
                continue;
            }
            let param = match &a.name {
                Some(n) => Some(n.name.as_str()),
                None => positions.get(i).copied(),
            };
            let element_slot = matches!(
                (name, i + offset),
                (
                    "array.push" | "array.unshift" | "array.fill" | "matrix.fill",
                    1
                ) | ("array.set" | "array.insert" | "map.put", 2)
                    | ("matrix.set", 3)
            );
            let expected = if element_slot {
                receiver_elem.clone()
            } else if (name == "array.new_bool"
                && (a.name.as_ref().is_some_and(|n| n.name == "initial_value")
                    || (a.name.is_none() && i == 1)))
                || param.is_some_and(|p| builtins::bool_param(name, p))
            {
                Some(Ty::Bool)
            } else {
                None
            };
            let (t, q) = self.expr_expect(&a.value, expected.as_ref());
            if expected == Some(Ty::Bool) {
                self.mark_bool_context(&a.value);
            }
            tys.push(t);
            quals.push(q);
        }
        (
            builtins::call_type(name, &tys),
            builtins::call_qual(name, &quals),
        )
    }
}

// -- helpers -------------------------------------------------------------------

fn join_opt(a: Option<&(Ty, Qual)>, b: Option<&(Ty, Qual)>) -> Ty {
    match (a, b) {
        (Some((a, _)), Some((b, _))) => a.join(b),
        (Some((t, _)), None) | (None, Some((t, _))) => t.clone(),
        (None, None) => Ty::Unknown,
    }
}

pub(crate) fn fn_id(def: &ast::FuncDef) -> String {
    if def.receiver.is_some() {
        method_id(&def.name.name)
    } else {
        def.name.name.clone()
    }
}

fn method_id(name: &str) -> String {
    format!("method {name}")
}

fn receiver_matches(def: &ast::FuncDef, receiver: &Ty) -> bool {
    let Some(rt) = &def.receiver else {
        return false;
    };
    let declared = field_type(rt);
    match (&declared, receiver) {
        (Ty::Array(_), Ty::Array(_)) => true,
        (a, b) => a == b || *b == Ty::Unknown,
    }
}

pub(crate) fn named_type(name: &str) -> Ty {
    match name {
        "int" => Ty::Int,
        "float" => Ty::Float,
        "bool" => Ty::Bool,
        "string" => Ty::Str,
        "color" => Ty::Color,
        other => Ty::Named(other.to_string()),
    }
}

pub(crate) fn field_type(ty: &ast::FieldType) -> Ty {
    match &ty.kind {
        ast::FieldTypeKind::Scalar(s) => scalar(*s),
        ast::FieldTypeKind::Color => Ty::Color,
        ast::FieldTypeKind::Array(elem) => Ty::Array(Box::new(field_type(elem))),
        ast::FieldTypeKind::Matrix(elem) => Ty::Matrix(Box::new(field_type(elem))),
        ast::FieldTypeKind::Map(key, value) => {
            Ty::Map(Box::new(field_type(key)), Box::new(field_type(value)))
        }
        ast::FieldTypeKind::Named(n) => named_type(n),
    }
}

pub(crate) fn annotation_type(ann: &ast::TypeAnnotation) -> Ty {
    match &ann.kind {
        ast::AnnotationKind::Scalar(s) => scalar(*s),
        ast::AnnotationKind::Color => Ty::Color,
        ast::AnnotationKind::Array(elem) => Ty::Array(Box::new(field_type(elem))),
        ast::AnnotationKind::Matrix(elem) => Ty::Matrix(Box::new(field_type(elem))),
        ast::AnnotationKind::Map(key, value) => {
            Ty::Map(Box::new(field_type(key)), Box::new(field_type(value)))
        }
        ast::AnnotationKind::Named(n) => named_type(n),
    }
}

fn scalar(s: ast::AnnotatedType) -> Ty {
    match s {
        ast::AnnotatedType::Int => Ty::Int,
        ast::AnnotatedType::Float => Ty::Float,
        ast::AnnotatedType::Bool => Ty::Bool,
        ast::AnnotatedType::String => Ty::Str,
    }
}

fn literal_type(expr: &Expr) -> Ty {
    match peel(expr) {
        Expr::Int { .. } => Ty::Int,
        Expr::Float { .. } => Ty::Float,
        Expr::Bool { .. } => Ty::Bool,
        Expr::Str { .. } => Ty::Str,
        Expr::Color { .. } => Ty::Color,
        _ => Ty::Unknown,
    }
}

fn loop_body(stmt: &Stmt) -> Option<&[Stmt]> {
    match stmt {
        Stmt::For(loop_stmt) => Some(&loop_stmt.body),
        Stmt::ForIn(loop_stmt) => Some(&loop_stmt.body),
        Stmt::While(loop_stmt) => Some(&loop_stmt.body),
        _ => None,
    }
}

/// Visit an expression's direct children (not into statement blocks).
pub(crate) fn for_each_child<'e>(expr: &'e Expr, f: &mut dyn FnMut(&'e Expr)) {
    match expr {
        Expr::Unary { operand, .. } => f(operand),
        Expr::Binary { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        Expr::Ternary {
            cond,
            then_e,
            else_e,
            ..
        } => {
            f(cond);
            f(then_e);
            f(else_e);
        }
        Expr::Call { args, .. } => {
            for a in args {
                f(&a.value);
            }
        }
        Expr::MethodCall { receiver, args, .. } => {
            f(receiver);
            for a in args {
                f(&a.value);
            }
        }
        Expr::History { target, offset, .. } => {
            f(target);
            f(offset);
        }
        Expr::Group { inner, .. } => f(inner),
        Expr::Tuple { elements, .. } => {
            for e in elements {
                f(e);
            }
        }
        Expr::If(i) => f(&i.cond),
        Expr::Switch(s) => {
            if let Some(subject) = &s.subject {
                f(subject);
            }
            for arm in &s.arms {
                if let Some(label) = &arm.label {
                    f(label);
                }
            }
        }
        Expr::Loop(stmt) => for_each_stmt_expr(stmt, f),
        _ => {}
    }
}

/// Visit the expressions a statement evaluates directly (headers and values,
/// not nested block statements).
pub(crate) fn for_each_stmt_expr<'e>(stmt: &'e Stmt, f: &mut dyn FnMut(&'e Expr)) {
    match stmt {
        Stmt::Assign { value, .. }
        | Stmt::Reassign { value, .. }
        | Stmt::Destructure { value, .. }
        | Stmt::Expr { expr: value, .. } => f(value),
        Stmt::If(i) => f(&i.cond),
        Stmt::Once(o) => {
            if let Some(cond) = &o.cond {
                f(cond);
            }
        }
        Stmt::For(l) => {
            f(&l.start);
            f(&l.end);
            if let Some(s) = &l.step {
                f(s);
            }
        }
        Stmt::ForIn(l) => f(&l.iterable),
        Stmt::While(w) => f(&w.cond),
        Stmt::Switch(s) => {
            if let Some(subject) = &s.subject {
                f(subject);
            }
            for arm in &s.arms {
                if let Some(label) = &arm.label {
                    f(label);
                }
            }
        }
        Stmt::Break { .. } | Stmt::Continue { .. } => {}
    }
}

/// The statement blocks nested directly in a statement.
pub(crate) fn child_blocks(stmt: &Stmt) -> Vec<&[Stmt]> {
    match stmt {
        Stmt::If(i) => {
            let mut v: Vec<&[Stmt]> = vec![&i.then_branch];
            if let Some(e) = &i.else_branch {
                v.push(e);
            }
            v
        }
        Stmt::For(f) => vec![&f.body],
        Stmt::Once(o) => vec![&o.body],
        Stmt::ForIn(f) => vec![&f.body],
        Stmt::While(w) => vec![&w.body],
        Stmt::Switch(s) => s.arms.iter().map(|a| a.body.as_slice()).collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_name_receiver_types_do_not_depend_on_hash_iteration_order() {
        let source = r#"//@version=5
indicator("Ambiguous receiver types")
type Alpha
    float value
type Beta
    float value
type Gamma
    float value
first() =>
    Alpha receiver = Alpha.new(close)
    receiver
second() =>
    Beta receiver = Beta.new(close)
    receiver
third() =>
    Gamma receiver = Gamma.new(close)
    receiver
"#;
        let parsed = pinewasm_parse::parse(source);
        assert!(
            parsed
                .diagnostics
                .iter()
                .all(|d| d.severity != pinewasm_span::Severity::Error),
            "{:?}",
            parsed.diagnostics
        );

        let model = Model::build(&parsed.program);
        let mut candidates = model
            .decls
            .values()
            .filter(|decl| decl.name == "receiver")
            .flat_map(|decl| decl.tys.iter().cloned())
            .collect::<Vec<_>>();
        candidates.sort_by_key(|ty| format!("{ty:?}"));
        candidates.dedup();
        assert_eq!(
            candidates,
            vec![
                Ty::Named("Alpha".into()),
                Ty::Named("Beta".into()),
                Ty::Named("Gamma".into())
            ]
        );

        // A fold over HashMap values could return any of the distinct UDTs that
        // follow a conflicting pair. The exact 3-way example has three possible
        // outputs depending on iteration order.
        let old_fold_outputs = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ]
        .map(|order| {
            order
                .into_iter()
                .fold(Ty::Unknown, |joined, index| joined.join(&candidates[index]))
        })
        .into_iter()
        .collect::<HashSet<_>>();
        assert_eq!(old_fold_outputs.len(), 3);

        for _ in 0..64 {
            assert_eq!(
                Model::build(&parsed.program).dotted_value_type("receiver"),
                Ty::Unknown
            );
        }
    }
}
