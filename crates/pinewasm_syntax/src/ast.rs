//! Abstract syntax tree for the Pine Script® surface that PineWASM supports.
//!
//! The AST is a faithful, span-carrying representation of the surface
//! syntax. It is untyped and unresolved; those properties are added when
//! `pinewasm_semantics` lowers it to HIR.

use pinewasm_span::Span;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Program {
    pub version: Option<VersionDirective>,
    /// The message text from a `//@strategy_alert_message=...` compiler
    /// annotation, if present (the last one wins). Stored as strategy metadata.
    pub strategy_alert_message: Option<String>,
    pub items: Vec<Item>,
    pub span: Span,
}

/// A `//@version=N` directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionDirective {
    pub version: u32,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Item {
    /// `indicator("Example")` — recognized syntactically at top level so the
    /// declaration is a first-class item; arguments are validated
    /// semantically.
    Indicator(IndicatorDecl),
    /// `strategy("Example", ...)` — the strategy analog of `indicator(...)`.
    /// Recognized syntactically at top level; arguments (the parameters
    /// PineWASM supports) are validated and const-folded into metadata during
    /// semantic analysis. A script declaring `strategy(...)` may not also
    /// declare `indicator(...)` (`E0240`).
    Strategy(StrategyDecl),
    /// `library("Title", overlay?)` — the library analog of `indicator(...)`.
    /// A script begins with `library(...)` instead of `indicator()` or
    /// `strategy()` when it is meant to be imported; only in a library script
    /// may a definition carry the `export` modifier (`E0243` otherwise). The
    /// two script kinds and `library(...)` are mutually exclusive (`E0240`).
    Library(LibraryDecl),
    /// `import <owner>/<name>/<version> as alias` — an import of a
    /// library. The publisher, name and version are parsed and used only to
    /// resolve a host-supplied library (no network); `as alias` is optional
    /// (defaults to the library name). The imported library's exported
    /// functions are called as `alias.fn(...)`.
    Import(ImportDecl),
    /// `name(p1, p2) => expr` or a multi-line `name(p1, p2) =>` NEWLINE INDENT
    /// body DEDENT — a user-defined function definition. Definitions are
    /// top-level only (a definition nested in a block is `E0224`); the body's
    /// last statement supplies the return value. Functions are monomorphized
    /// per call site during semantic analysis (inline expansion), so no
    /// HIR/MIR/CSIL node represents a function or a call frame.
    FuncDef(FuncDef),
    /// `enum Name` followed by an indented list of members (`buy = "Buy"`,
    /// `sell`). A top-level declaration: enum members are referenced as
    /// `Name.member` and form a distinct value type. Titles are optional and
    /// default to the member name when omitted.
    EnumDef(EnumDecl),
    /// `type Name` followed by an indented list of typed field declarations
    /// (`float x`, `int y = 5`, `MyType child`, `array<float> xs`). A top-level
    /// declaration: a user-defined type whose instances are constructed
    /// with `Name.new(...)` / `Name.copy(obj)` and whose fields are read/written
    /// through `obj.field`. Objects are reference values on the VM heap.
    TypeDef(TypeDecl),
    Stmt(Stmt),
}

impl Item {
    pub fn span(&self) -> Span {
        match self {
            Item::Indicator(d) => d.span,
            Item::Strategy(d) => d.span,
            Item::Library(d) => d.span,
            Item::Import(d) => d.span,
            Item::FuncDef(f) => f.span,
            Item::EnumDef(e) => e.span,
            Item::TypeDef(t) => t.span,
            Item::Stmt(s) => s.span(),
        }
    }
}

/// A user-defined type declaration (`type Name` + an indented field list).
/// Each field has a declared type, a name, and an optional default-value
/// expression (`int y = 5`). Fields are stored in declaration order (their
/// order is their runtime slot order). Duplicate type name, duplicate field
/// name, and unknown field type are semantic errors (`E0236`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypeDecl {
    pub name: Ident,
    pub fields: Vec<TypeField>,
    /// `true` when the declaration carried the `export` modifier. Only
    /// meaningful in a `library(...)` script; `E0243` in an indicator/strategy.
    pub exported: bool,
    pub span: Span,
}

/// One field of a [`TypeDecl`]: a declared type, a name, and an optional
/// default value. When the default is absent the field takes `na` (or the
/// declared field type's default) at construction unless the constructor
/// supplies it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypeField {
    pub ty: FieldType,
    pub name: Ident,
    pub default: Option<Expr>,
    pub span: Span,
}

/// The declared type of a [`TypeField`], as written in the surface syntax.
/// The scalar keywords and `color` map to the corresponding value types;
/// `array<T>`, `matrix<T>`, and `map<K, V>` are collection fields; a bare identifier names another
/// user-defined type or enum (resolved semantically — an unknown name is
/// `E0236`). Kept as surface syntax; `pinewasm_semantics` resolves it to a
/// `Type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldType {
    pub kind: FieldTypeKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FieldTypeKind {
    /// One of the scalar keywords: `int`, `float`, `bool`, `string`.
    Scalar(AnnotatedType),
    /// The `color` keyword.
    Color,
    /// `array<T>` (or the `T[]` sugar) where `T` is any supported element type.
    Array(Box<FieldType>),
    /// `matrix<T>` where `T` is any supported element type.
    Matrix(Box<FieldType>),
    /// `map<K, V>` where `K` is a value type and `V` is any supported value type.
    Map(Box<FieldType>, Box<FieldType>),
    /// A bare identifier naming another user-defined type or enum.
    Named(String),
}

impl FieldType {
    /// A short human-readable rendering of the surface type (used in AST
    /// dumps and diagnostics): `int`, `color`, `array<float>`, `MyType`.
    pub fn describe(&self) -> String {
        match &self.kind {
            FieldTypeKind::Scalar(a) => a.keyword().to_string(),
            FieldTypeKind::Color => "color".to_string(),
            FieldTypeKind::Array(elem) => format!("array<{}>", elem.describe()),
            FieldTypeKind::Matrix(elem) => format!("matrix<{}>", elem.describe()),
            FieldTypeKind::Map(key, value) => {
                format!("map<{}, {}>", key.describe(), value.describe())
            }
            FieldTypeKind::Named(name) => name.clone(),
        }
    }
}

/// A user-defined enum declaration (`enum Name` + an indented member list).
/// Each member has a name and an optional title (the `= "..."` string shown in
/// UI dropdowns; when absent the member name is the title). Members are
/// distinct values of the enum type; their order is their declaration order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnumDecl {
    pub name: Ident,
    pub variants: Vec<EnumVariant>,
    /// `true` when the declaration carried the `export` modifier. Only
    /// meaningful in a `library(...)` script; `E0243` in an indicator/strategy.
    pub exported: bool,
    pub span: Span,
}

/// One member of an [`EnumDecl`]: a name and an optional title string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnumVariant {
    pub name: Ident,
    /// The optional `= "title"` string; `None` means the title defaults to the
    /// member name.
    pub title: Option<String>,
    pub span: Span,
}

/// A user-defined function definition (`name(p1, p2 = 1) => body`). `params`
/// are plain identifiers with optional default-value expressions; `body` is
/// the statement list whose **last** statement supplies the return value (a
/// single-line `=> expr` body is a one-element list holding that expression
/// statement).
///
/// A **method** (`method f(TypeName this, ...) => body`) is a `FuncDef`
/// whose `receiver` is `Some(ty)`: the declared type of its first parameter
/// (`params[0]`), which is the receiver the method associates with. A method
/// registers so that `obj.f(args)` resolves — the receiver becomes the first
/// argument, reusing the ordinary per-call-site inlining machinery. A plain
/// function has `receiver == None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FuncDef {
    pub name: Ident,
    pub params: Vec<FuncParam>,
    pub body: Vec<Stmt>,
    /// `Some(ty)` when this definition was declared with the `method` keyword:
    /// `ty` is the explicitly-typed first parameter (the receiver type). `None`
    /// for a plain function.
    pub receiver: Option<FieldType>,
    /// `true` when the definition carried the `export` modifier. Only
    /// meaningful in a `library(...)` script; `E0243` in an indicator/strategy.
    /// An exported function is callable as `alias.name(...)` from an importer.
    pub exported: bool,
    pub span: Span,
}

/// One parameter of a [`FuncDef`]: an identifier with an optional declared
/// type (`float x`, `array<color> shades`, `simple int n` — the qualifier is
/// retained on the AST) and an optional default value
/// (`p = 1`). A parameter with a default may be omitted at a call site.
/// Untyped parameters infer their type from each call site. Declared types
/// constrain arguments and defaults, supply the expected type for `na`, and
/// permit the scalar int-to-float widening performed by semantic analysis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FuncParam {
    pub name: Ident,
    /// Explicit Pine qualifier, when present before the declared type.
    #[serde(default)]
    pub qualifier: Option<ParamQualifier>,
    /// The declared parameter type, when written (`float x`); `None` for a
    /// plain untyped parameter.
    pub ty: Option<FieldType>,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParamQualifier {
    Const,
    Input,
    Simple,
    Series,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndicatorDecl {
    pub args: Vec<CallArg>,
    pub span: Span,
}

/// `strategy("Title", ...)` top-level declaration. Structurally identical
/// to [`IndicatorDecl`] (a raw argument list validated semantically); a
/// distinct AST node so the semantic analyzer can apply the strategy signature
/// and the `strategy()`/`indicator()` mutual-exclusion rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyDecl {
    pub args: Vec<CallArg>,
    pub span: Span,
}

/// `library("Title", overlay?)` top-level declaration. Structurally like
/// [`IndicatorDecl`] (a raw argument list validated semantically: a constant
/// string title and an optional `overlay` boolean); a distinct AST node so the
/// analyzer marks the script as a *library* and permits `export`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryDecl {
    pub args: Vec<CallArg>,
    pub span: Span,
}

/// `import Publisher/LibName/Version as alias`. The publisher, library
/// name, and version are the map-lookup key; `alias` is the local namespace
/// through which the library's exported functions are called (`alias.fn(...)`).
/// When `as alias` is omitted the alias defaults to the library name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportDecl {
    pub publisher: Ident,
    pub library: Ident,
    /// The version component (`1`, `2`, ...). Parsed as an identifier-or-number
    /// token; carried verbatim for the map lookup.
    pub version: String,
    /// The local alias (`as name`), or the library name when `as` is omitted.
    pub alias: Ident,
    pub span: Span,
}

/// One argument at a call site. `name` is `Some` for a named argument
/// (`length = 14`) and `None` for a positional one. Positional arguments may
/// not follow a named argument (a parse error); the registry (and user
/// functions) bind positionals by order, then named by parameter name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallArg {
    pub name: Option<Ident>,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

/// A declared type on an annotated assignment: `int x = 1` or `line l = na`.
/// A scalar keyword sets the variable's declared type explicitly (independent
/// of the initializer, subject to the E0226 compatibility rules); a bare type
/// name (`line`, `box`, `chart.point`, ...) names a drawing object type. Only
/// the known drawing type names are recognized in this position by the parser,
/// so `pinewasm_semantics` resolves a `Named` annotation as a total mapping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypeAnnotation {
    pub kind: AnnotationKind,
    /// An explicitly declared qualifier; omitted declarations infer it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualifier: Option<TypeQualifier>,
    pub span: Span,
}

/// Qualifier keywords permitted on variable declarations. `input` is inferred
/// from input calls, not a variable-declaration keyword.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeQualifier {
    Const,
    Simple,
    Series,
}

/// The declared type of a [`TypeAnnotation`]: a scalar keyword, `color`, an
/// collection type (`array<T>` / `T[]`, `matrix<T>`, or `map<K, V>`), or a named object type — a drawing type
/// such as `line`/`box`/`chart.point`, or a user-defined type/enum name.
/// Named annotations are resolved by `pinewasm_semantics` (an unknown name
/// is `E0236`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AnnotationKind {
    /// One of the scalar keywords: `int`, `float`, `bool`, `string`.
    Scalar(AnnotatedType),
    /// The `color` type name.
    Color,
    /// `array<T>` or the `T[]` sugar.
    Array(Box<FieldType>),
    /// `matrix<T>`.
    Matrix(Box<FieldType>),
    /// `map<K, V>`.
    Map(Box<FieldType>, Box<FieldType>),
    /// A bare type name: a drawing object type (`line`, `label`, `box`,
    /// `table`, `linefill`, `polyline`, `chart.point`) or a user-defined
    /// type/enum.
    Named(String),
}

impl AnnotationKind {
    /// A short human-readable rendering of the declared type (used in AST
    /// dumps): the scalar keyword or the bare type name.
    pub fn describe(&self) -> String {
        match self {
            AnnotationKind::Scalar(ty) => ty.keyword().to_string(),
            AnnotationKind::Color => "color".to_string(),
            AnnotationKind::Array(elem) => format!("array<{}>", elem.describe()),
            AnnotationKind::Matrix(elem) => format!("matrix<{}>", elem.describe()),
            AnnotationKind::Map(key, value) => {
                format!("map<{}, {}>", key.describe(), value.describe())
            }
            AnnotationKind::Named(name) => name.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnnotatedType {
    Int,
    Float,
    Bool,
    String,
}

impl AnnotatedType {
    pub fn keyword(self) -> &'static str {
        match self {
            AnnotatedType::Int => "int",
            AnnotatedType::Float => "float",
            AnnotatedType::Bool => "bool",
            AnnotatedType::String => "string",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Stmt {
    /// `x = expr` (declaration), or `var x = expr` when `is_var` is true
    /// (persistent declaration: initialized once, carried across bars), or
    /// `varip x = expr` when `is_varip` is true (persistent AND intrabar —
    /// survives realtime-tick rollback; see docs/runtime.md). `is_varip`
    /// implies `is_var`. An optional leading `annotation` (`int x = 1`) fixes
    /// the declared type.
    Assign {
        name: Ident,
        value: Expr,
        is_var: bool,
        is_varip: bool,
        annotation: Option<TypeAnnotation>,
        span: Span,
    },
    /// `x := expr` — reassignment of an existing variable.
    Reassign {
        name: Ident,
        value: Expr,
        span: Span,
    },
    /// `[a, b, c] = expr` — a destructuring declaration. `targets` are the
    /// plain identifiers to declare (two or more), bound left-to-right to the
    /// elements of the tuple `value` produces (a tuple-returning `ta.*` call
    /// or a user function whose body ends in a tuple expression). Always
    /// declares with `=`; the number of targets must match the tuple arity
    /// (`E0225` otherwise).
    Destructure {
        targets: Vec<Ident>,
        value: Expr,
        span: Span,
    },
    /// A bare expression on its own line, e.g. `plot(x)`.
    Expr { expr: Expr, span: Span },
    /// `if cond ... else ...` with indented bodies.
    If(IfStmt),
    /// `once [condition]` with an indented effect-only body.
    Once(OnceStmt),
    /// `for i = start to end [by step]` with an indented body. Boxed: it is
    /// the largest `Stmt` variant (a counter plus three bound expressions and
    /// a body), so keeping it behind a pointer keeps `Stmt` compact.
    For(Box<ForStmt>),
    /// `for element in arr` / `for [index, element] in arr` with an indented
    /// body. Boxed to keep `Stmt` compact. Desugars during semantic analysis
    /// to a counted loop over `array.size(arr)` that binds the element (and
    /// optional index) via `array.get` per iteration.
    ForIn(Box<ForInStmt>),
    /// `while cond` with an indented body.
    While(WhileStmt),
    /// `switch ...` in statement position (each arm body is a block of
    /// statements; no value is produced).
    Switch(SwitchStmt),
    /// `break` — exit the innermost loop.
    Break { span: Span },
    /// `continue` — skip to the next iteration of the innermost loop.
    Continue { span: Span },
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Assign { span, .. }
            | Stmt::Reassign { span, .. }
            | Stmt::Destructure { span, .. }
            | Stmt::Expr { span, .. }
            | Stmt::Break { span }
            | Stmt::Continue { span } => *span,
            Stmt::If(i) => i.span,
            Stmt::Once(o) => o.span,
            Stmt::For(f) => f.span,
            Stmt::ForIn(f) => f.span,
            Stmt::While(w) => w.span,
            Stmt::Switch(s) => s.span,
        }
    }
}

/// A `switch` in statement or expression position. When `subject` is present
/// (`switch expr`), each non-default arm carries a constant label compared to
/// the subject with `==`; when absent (subjectless `switch`), each arm label
/// is a boolean condition. A `default` arm (an arm with no label) has
/// `label == None`. There is no fallthrough. In expression position the last
/// statement of the taken arm's body supplies the value; in statement
/// position the body is executed for effect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwitchStmt {
    /// The matched subject (`switch subject`), or `None` for a subjectless
    /// switch whose arms are boolean conditions.
    pub subject: Option<Expr>,
    pub arms: Vec<SwitchArm>,
    pub span: Span,
}

/// One arm of a `switch`. `label` is the case value / condition, or `None`
/// for the `default` arm. `body` is the arm's block: a single inline
/// expression statement (`=> expr`) or an indented block whose last statement
/// supplies the value in expression position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwitchArm {
    pub label: Option<Expr>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IfStmt {
    pub cond: Expr,
    pub then_branch: Vec<Stmt>,
    pub else_branch: Option<Vec<Stmt>>,
    pub span: Span,
}

/// A conditional effect block that becomes inactive after its first
/// execution on a closed bar. Its activation state participates in ordinary
/// realtime rollback. An omitted condition means `true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OnceStmt {
    pub cond: Option<Expr>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// `for counter = start to end [by step]` with an indented body. The start
/// and step expressions are evaluated once at loop entry; the end expression
/// is evaluated at entry for direction selection and again before each
/// iteration to determine whether the loop continues.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForStmt {
    pub counter: Ident,
    pub start: Expr,
    pub end: Expr,
    /// The optional `by step` expression; `None` means the implicit step
    /// (+1 when start <= end, else -1).
    pub step: Option<Expr>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// `for element in arr` / `for [index, element] in arr` with an indented body.
/// `index` is `Some` only for the tuple form. The iterable is evaluated once
/// at loop entry; the element (and index) are loop-local bindings rebound each
/// iteration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForInStmt {
    /// The `[index, element]` tuple form's index binding, or `None` for the
    /// bare `element` form.
    pub index: Option<Ident>,
    pub element: Ident,
    pub iterable: Expr,
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// `while cond` with an indented body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WhileStmt {
    pub cond: Expr,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    /// `%` — remainder (modulo).
    Rem,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

impl BinaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Rem => "%",
            BinaryOp::Lt => "<",
            BinaryOp::Le => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::Ge => ">=",
            BinaryOp::Eq => "==",
            BinaryOp::Ne => "!=",
            BinaryOp::And => "and",
            BinaryOp::Or => "or",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnaryOp {
    Neg,
    /// Unary `+`: identity on a numeric operand (`+x == x`). It
    /// carries no runtime effect, so the checker folds it away rather than
    /// emitting an HIR unary node.
    Pos,
    Not,
}

impl UnaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            UnaryOp::Neg => "-",
            UnaryOp::Pos => "+",
            UnaryOp::Not => "not",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    /// An int-form number literal (`1`, not `1.0`), carried exactly as an
    /// `i64` so values above 2^53 never round through `f64`. History
    /// offsets and `ta.*` lengths require this form.
    Int {
        value: i64,
        span: Span,
    },
    /// A float-form number literal (`1.5`, `0.25`).
    Float {
        value: f64,
        span: Span,
    },
    Bool {
        value: bool,
        span: Span,
    },
    /// The `na` literal (keyword `na` **not** followed by `(`; `na(...)` is
    /// a call to the `na` builtin). Untyped in the AST; the checker adopts
    /// the type expected by the surrounding context (`E0220` when none).
    Na {
        span: Span,
    },
    Str {
        /// The decoded value (escapes resolved, quotes stripped).
        value: String,
        span: Span,
    },
    /// A hex color literal (`#RRGGBB`/`#RRGGBBAA`). `t` is transparency in
    /// `0..=100`, derived from the optional alpha byte:
    /// `t = 100 * (1 - alpha / 255)`. Absent alpha ⇒ opaque, `t = 0`.
    Color {
        r: u8,
        g: u8,
        b: u8,
        #[serde(serialize_with = "serialize_transparency")]
        t: f64,
        span: Span,
    },
    Ident(Ident),
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
        span: Span,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    /// `cond ? then_e : else_e` — the ternary conditional. Lowest
    /// precedence, right-associative (`a ? b : c ? d : e` is
    /// `a ? b : (c ? d : e)`).
    Ternary {
        cond: Box<Expr>,
        then_e: Box<Expr>,
        else_e: Box<Expr>,
        span: Span,
    },
    /// `callee(args...)` — direct callees are identifiers (including
    /// namespace-qualified names). Arguments may
    /// be positional or `name = value` (see [`CallArg`]).
    Call {
        callee: Ident,
        args: Vec<CallArg>,
        /// Generic-constructor type arguments carried structurally
        /// (`array.new<Point>` → callee `array.new`, type args `[Point]`;
        /// `map.new<string, Point>` → `[string, Point]`) so semantics
        /// dispatches on node shape — never on strings encoded into
        /// `callee.name`. Empty for ordinary calls; all-scalar generics
        /// (`array.new<float>`) are rewritten by the parser to their named
        /// registry constructors (`array.new_float`) and carry no type args.
        type_args: Vec<Ident>,
        span: Span,
    },
    /// `receiver.method(args...)` — a method call whose receiver is an
    /// arbitrary expression (chaining, e.g. `obj.a().b()`). A method call
    /// off a bare variable (`obj.a(...)`) is folded into a dotted [`Expr::Call`] in
    /// the primary parser and resolved as a method there; this node exists so a
    /// `.method(...)` can chain off the result of a preceding call or index.
    MethodCall {
        receiver: Box<Expr>,
        method: Ident,
        args: Vec<CallArg>,
        span: Span,
    },
    /// `expression.field` — read a field from an arbitrary expression that
    /// evaluates to an object, e.g. `array.get(points, 0).price`. Dotted names
    /// that begin with an identifier remain folded into [`Expr::Ident`].
    FieldAccess {
        receiver: Box<Expr>,
        field: Ident,
        span: Span,
    },
    /// `target[offset]` history reference.
    History {
        target: Box<Expr>,
        offset: Box<Expr>,
        span: Span,
    },
    /// A parenthesized expression, kept so debug dumps mirror the source.
    Group {
        inner: Box<Expr>,
        span: Span,
    },
    /// `if cond ... else ...` used as a value: each branch is an indented
    /// block whose last statement is an expression supplying the branch
    /// value. Boxed to keep `Expr` compact. Structurally identical to the
    /// statement form; the value is extracted from the trailing expression
    /// statement during semantic analysis.
    If(Box<IfExpr>),
    /// `switch ...` used as a value. Boxed to keep `Expr` compact.
    Switch(Box<SwitchStmt>),
    /// A loop used as a value; the final expression evaluated by its body
    /// supplies the result. The statement is boxed because loop bodies
    /// contain expressions that can themselves contain loops.
    Loop(Box<Stmt>),
    /// `[e0, e1, ...]` — a tuple expression (two or more elements). Legal
    /// only as the RHS of a destructuring declaration (`[a, b] = f(...)`) or
    /// as the trailing value of a user-function body; anywhere else it is a
    /// semantic error (`E0225`). Distinct from the `target[offset]` history
    /// postfix, which starts with a `[` only after a primary.
    Tuple {
        elements: Vec<Expr>,
        span: Span,
    },
}

/// `if cond ... else ...` in expression position. Same shape as [`IfStmt`];
/// kept distinct so consumers can tell an if used as a value from one used
/// as a statement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IfExpr {
    pub cond: Expr,
    pub then_branch: Vec<Stmt>,
    pub else_branch: Option<Vec<Stmt>>,
    pub span: Span,
}

impl Expr {
    /// Walk every identifier *read* in this expression tree — variable
    /// references and call callees, dotted names unsplit — in source order.
    /// Used for dependency analysis (e.g. pruning unused library globals).
    pub fn for_each_ident(&self, f: &mut dyn FnMut(&Ident)) {
        match self {
            Expr::Int { .. }
            | Expr::Float { .. }
            | Expr::Bool { .. }
            | Expr::Na { .. }
            | Expr::Str { .. }
            | Expr::Color { .. } => {}
            Expr::Ident(ident) => f(ident),
            Expr::Unary { operand, .. } => operand.for_each_ident(f),
            Expr::Binary { lhs, rhs, .. } => {
                lhs.for_each_ident(f);
                rhs.for_each_ident(f);
            }
            Expr::Ternary {
                cond,
                then_e,
                else_e,
                ..
            } => {
                cond.for_each_ident(f);
                then_e.for_each_ident(f);
                else_e.for_each_ident(f);
            }
            Expr::Call { callee, args, .. } => {
                f(callee);
                for arg in args {
                    arg.value.for_each_ident(f);
                }
            }
            Expr::MethodCall { receiver, args, .. } => {
                receiver.for_each_ident(f);
                for arg in args {
                    arg.value.for_each_ident(f);
                }
            }
            Expr::FieldAccess { receiver, .. } => receiver.for_each_ident(f),
            Expr::History { target, offset, .. } => {
                target.for_each_ident(f);
                offset.for_each_ident(f);
            }
            Expr::Group { inner, .. } => inner.for_each_ident(f),
            Expr::If(if_expr) => {
                if_expr.cond.for_each_ident(f);
                for s in &if_expr.then_branch {
                    s.for_each_read_ident(f);
                }
                for s in if_expr.else_branch.iter().flatten() {
                    s.for_each_read_ident(f);
                }
            }
            Expr::Switch(switch) => {
                if let Some(subject) = &switch.subject {
                    subject.for_each_ident(f);
                }
                for arm in &switch.arms {
                    if let Some(label) = &arm.label {
                        label.for_each_ident(f);
                    }
                    for s in &arm.body {
                        s.for_each_read_ident(f);
                    }
                }
            }
            Expr::Loop(stmt) => stmt.for_each_read_ident(f),
            Expr::Tuple { elements, .. } => {
                for e in elements {
                    e.for_each_ident(f);
                }
            }
        }
    }
}

impl Stmt {
    /// Walk every identifier *read* by this statement (and its nested
    /// blocks), in source order. Assignment/loop-binding targets are not
    /// reads and are not visited (see [`Stmt::for_each_assigned_name`]).
    pub fn for_each_read_ident(&self, f: &mut dyn FnMut(&Ident)) {
        match self {
            Stmt::Assign { value, .. } | Stmt::Reassign { value, .. } => value.for_each_ident(f),
            Stmt::Destructure { value, .. } => value.for_each_ident(f),
            Stmt::Expr { expr, .. } => expr.for_each_ident(f),
            Stmt::If(if_stmt) => {
                if_stmt.cond.for_each_ident(f);
                for s in &if_stmt.then_branch {
                    s.for_each_read_ident(f);
                }
                for s in if_stmt.else_branch.iter().flatten() {
                    s.for_each_read_ident(f);
                }
            }
            Stmt::Once(once) => {
                if let Some(cond) = &once.cond {
                    cond.for_each_ident(f);
                }
                for s in &once.body {
                    s.for_each_read_ident(f);
                }
            }
            Stmt::For(for_stmt) => {
                for_stmt.start.for_each_ident(f);
                for_stmt.end.for_each_ident(f);
                if let Some(step) = &for_stmt.step {
                    step.for_each_ident(f);
                }
                for s in &for_stmt.body {
                    s.for_each_read_ident(f);
                }
            }
            Stmt::ForIn(for_in) => {
                for_in.iterable.for_each_ident(f);
                for s in &for_in.body {
                    s.for_each_read_ident(f);
                }
            }
            Stmt::While(while_stmt) => {
                while_stmt.cond.for_each_ident(f);
                for s in &while_stmt.body {
                    s.for_each_read_ident(f);
                }
            }
            Stmt::Switch(switch) => {
                if let Some(subject) = &switch.subject {
                    subject.for_each_ident(f);
                }
                for arm in &switch.arms {
                    if let Some(label) = &arm.label {
                        label.for_each_ident(f);
                    }
                    for s in &arm.body {
                        s.for_each_read_ident(f);
                    }
                }
            }
            Stmt::Break { .. } | Stmt::Continue { .. } => {}
        }
    }

    /// Walk the name of every variable this statement declares or writes —
    /// `Assign`/`Reassign` targets, destructure targets, and the same inside
    /// nested blocks. Loop counters and `for..in` bindings are block-local,
    /// never module-level, so they are not visited.
    pub fn for_each_assigned_name(&self, f: &mut dyn FnMut(&Ident)) {
        match self {
            Stmt::Assign { name, .. } | Stmt::Reassign { name, .. } => f(name),
            Stmt::Destructure { targets, .. } => {
                for t in targets {
                    f(t);
                }
            }
            Stmt::Expr { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
            Stmt::If(if_stmt) => {
                for s in if_stmt
                    .then_branch
                    .iter()
                    .chain(if_stmt.else_branch.iter().flatten())
                {
                    s.for_each_assigned_name(f);
                }
            }
            Stmt::Once(once) => {
                for s in &once.body {
                    s.for_each_assigned_name(f);
                }
            }
            Stmt::For(for_stmt) => {
                for s in &for_stmt.body {
                    s.for_each_assigned_name(f);
                }
            }
            Stmt::ForIn(for_in) => {
                for s in &for_in.body {
                    s.for_each_assigned_name(f);
                }
            }
            Stmt::While(while_stmt) => {
                for s in &while_stmt.body {
                    s.for_each_assigned_name(f);
                }
            }
            Stmt::Switch(switch) => {
                for arm in &switch.arms {
                    for s in &arm.body {
                        s.for_each_assigned_name(f);
                    }
                }
            }
        }
    }

    /// Walk the name of every variable this statement *reassigns* (`x := e`),
    /// including inside nested blocks. Unlike [`Self::for_each_assigned_name`]
    /// this skips `=` declarations: inside a function body a `=` binds a fresh
    /// local, but a `:=` targets an outer/module variable — so this identifies
    /// the module globals a function writes (library-global pruning).
    pub fn for_each_reassigned_name(&self, f: &mut dyn FnMut(&Ident)) {
        match self {
            Stmt::Reassign { name, .. } => f(name),
            Stmt::Assign { .. }
            | Stmt::Destructure { .. }
            | Stmt::Expr { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => {}
            Stmt::If(if_stmt) => {
                for s in if_stmt
                    .then_branch
                    .iter()
                    .chain(if_stmt.else_branch.iter().flatten())
                {
                    s.for_each_reassigned_name(f);
                }
            }
            Stmt::Once(once) => {
                for s in &once.body {
                    s.for_each_reassigned_name(f);
                }
            }
            Stmt::For(for_stmt) => {
                for s in &for_stmt.body {
                    s.for_each_reassigned_name(f);
                }
            }
            Stmt::ForIn(for_in) => {
                for s in &for_in.body {
                    s.for_each_reassigned_name(f);
                }
            }
            Stmt::While(while_stmt) => {
                for s in &while_stmt.body {
                    s.for_each_reassigned_name(f);
                }
            }
            Stmt::Switch(switch) => {
                for arm in &switch.arms {
                    for s in &arm.body {
                        s.for_each_reassigned_name(f);
                    }
                }
            }
        }
    }
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Int { span, .. }
            | Expr::Float { span, .. }
            | Expr::Bool { span, .. }
            | Expr::Na { span }
            | Expr::Str { span, .. }
            | Expr::Color { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Ternary { span, .. }
            | Expr::Call { span, .. }
            | Expr::MethodCall { span, .. }
            | Expr::FieldAccess { span, .. }
            | Expr::History { span, .. }
            | Expr::Group { span, .. }
            | Expr::Tuple { span, .. } => *span,
            Expr::Ident(i) => i.span,
            Expr::If(i) => i.span,
            Expr::Switch(s) => s.span,
            Expr::Loop(stmt) => stmt.span(),
        }
    }
}

// ---------------------------------------------------------------------------
// Debug dump
// ---------------------------------------------------------------------------

/// Render the AST as an indented tree with spans, for snapshot tests and the
/// pipeline-inspection clients.
pub fn dump_program(program: &Program) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Program @ {}", fmt_span(program.span));
    match &program.version {
        Some(v) => {
            let _ = writeln!(out, "  Version {} @ {}", v.version, fmt_span(v.span));
        }
        None => {
            let _ = writeln!(out, "  Version <none>");
        }
    }
    for item in &program.items {
        match item {
            Item::Indicator(decl) => {
                let _ = writeln!(out, "  IndicatorDecl @ {}", fmt_span(decl.span));
                for arg in &decl.args {
                    dump_call_arg(&mut out, arg, 2);
                }
            }
            Item::Strategy(decl) => {
                let _ = writeln!(out, "  StrategyDecl @ {}", fmt_span(decl.span));
                for arg in &decl.args {
                    dump_call_arg(&mut out, arg, 2);
                }
            }
            Item::Library(decl) => {
                let _ = writeln!(out, "  LibraryDecl @ {}", fmt_span(decl.span));
                for arg in &decl.args {
                    dump_call_arg(&mut out, arg, 2);
                }
            }
            Item::Import(decl) => {
                let _ = writeln!(
                    out,
                    "  Import `{}/{}/{}` as `{}` @ {}",
                    decl.publisher.name,
                    decl.library.name,
                    decl.version,
                    decl.alias.name,
                    fmt_span(decl.span)
                );
            }
            Item::FuncDef(def) => {
                let params: Vec<String> = def
                    .params
                    .iter()
                    .map(|p| {
                        if p.default.is_some() {
                            format!("{}=…", p.name.name)
                        } else {
                            p.name.name.clone()
                        }
                    })
                    .collect();
                let export_prefix = if def.exported { "export " } else { "" };
                match &def.receiver {
                    Some(recv) => {
                        let _ = writeln!(
                            out,
                            "  {export_prefix}Method `{}`[{}]({}) @ {}",
                            def.name.name,
                            fmt_field_type(recv),
                            params.join(", "),
                            fmt_span(def.span)
                        );
                    }
                    None => {
                        let _ = writeln!(
                            out,
                            "  {export_prefix}FuncDef `{}`({}) @ {}",
                            def.name.name,
                            params.join(", "),
                            fmt_span(def.span)
                        );
                    }
                }
                for p in &def.params {
                    if let Some(default) = &p.default {
                        indent(&mut out, 2);
                        let _ = writeln!(out, "Default `{}`", p.name.name);
                        dump_expr(&mut out, default, 3);
                    }
                }
                indent(&mut out, 2);
                out.push_str("Body\n");
                for s in &def.body {
                    dump_stmt(&mut out, s, 3);
                }
            }
            Item::EnumDef(decl) => {
                let export_prefix = if decl.exported { "export " } else { "" };
                let _ = writeln!(
                    out,
                    "  {export_prefix}EnumDef `{}` @ {}",
                    decl.name.name,
                    fmt_span(decl.span)
                );
                for v in &decl.variants {
                    indent(&mut out, 2);
                    match &v.title {
                        Some(title) => {
                            let _ = writeln!(
                                out,
                                "Variant `{}` = {:?} @ {}",
                                v.name.name,
                                title,
                                fmt_span(v.span)
                            );
                        }
                        None => {
                            let _ =
                                writeln!(out, "Variant `{}` @ {}", v.name.name, fmt_span(v.span));
                        }
                    }
                }
            }
            Item::TypeDef(decl) => {
                let export_prefix = if decl.exported { "export " } else { "" };
                let _ = writeln!(
                    out,
                    "  {export_prefix}TypeDef `{}` @ {}",
                    decl.name.name,
                    fmt_span(decl.span)
                );
                for f in &decl.fields {
                    indent(&mut out, 2);
                    let _ = writeln!(
                        out,
                        "Field `{}` : {} @ {}",
                        f.name.name,
                        fmt_field_type(&f.ty),
                        fmt_span(f.span)
                    );
                    if let Some(default) = &f.default {
                        dump_expr(&mut out, default, 3);
                    }
                }
            }
            Item::Stmt(stmt) => dump_stmt(&mut out, stmt, 1),
        }
    }
    out
}

/// Render a field's declared type for the AST dump.
fn fmt_field_type(ty: &FieldType) -> String {
    ty.describe()
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn fmt_span(span: Span) -> String {
    format!("{}..{}", span.start, span.end)
}

fn dump_stmt(out: &mut String, stmt: &Stmt, depth: usize) {
    match stmt {
        Stmt::Assign {
            name,
            value,
            is_var,
            is_varip,
            annotation,
            span,
        } => {
            indent(out, depth);
            let label = if *is_varip {
                "VaripAssign"
            } else if *is_var {
                "VarAssign"
            } else {
                "Assign"
            };
            match annotation {
                Some(a) => {
                    let _ = writeln!(
                        out,
                        "{label} `{}` : {} @ {}",
                        name.name,
                        a.kind.describe(),
                        fmt_span(*span)
                    );
                }
                None => {
                    let _ = writeln!(out, "{label} `{}` @ {}", name.name, fmt_span(*span));
                }
            }
            dump_expr(out, value, depth + 1);
        }
        Stmt::Reassign { name, value, span } => {
            indent(out, depth);
            let _ = writeln!(out, "Reassign `{}` @ {}", name.name, fmt_span(*span));
            dump_expr(out, value, depth + 1);
        }
        Stmt::Destructure {
            targets,
            value,
            span,
        } => {
            indent(out, depth);
            let names: Vec<&str> = targets.iter().map(|t| t.name.as_str()).collect();
            let _ = writeln!(
                out,
                "Destructure [{}] @ {}",
                names.join(", "),
                fmt_span(*span)
            );
            dump_expr(out, value, depth + 1);
        }
        Stmt::Expr { expr, span } => {
            indent(out, depth);
            let _ = writeln!(out, "ExprStmt @ {}", fmt_span(*span));
            dump_expr(out, expr, depth + 1);
        }
        Stmt::If(if_stmt) => {
            indent(out, depth);
            let _ = writeln!(out, "If @ {}", fmt_span(if_stmt.span));
            indent(out, depth + 1);
            out.push_str("Cond\n");
            dump_expr(out, &if_stmt.cond, depth + 2);
            indent(out, depth + 1);
            out.push_str("Then\n");
            for s in &if_stmt.then_branch {
                dump_stmt(out, s, depth + 2);
            }
            if let Some(else_branch) = &if_stmt.else_branch {
                indent(out, depth + 1);
                out.push_str("Else\n");
                for s in else_branch {
                    dump_stmt(out, s, depth + 2);
                }
            }
        }
        Stmt::Once(once) => {
            indent(out, depth);
            let _ = writeln!(out, "Once @ {}", fmt_span(once.span));
            if let Some(cond) = &once.cond {
                indent(out, depth + 1);
                out.push_str("Cond\n");
                dump_expr(out, cond, depth + 2);
            }
            for s in &once.body {
                dump_stmt(out, s, depth + 1);
            }
        }
        Stmt::For(for_stmt) => {
            indent(out, depth);
            let _ = writeln!(
                out,
                "For `{}` @ {}",
                for_stmt.counter.name,
                fmt_span(for_stmt.span)
            );
            indent(out, depth + 1);
            out.push_str("Start\n");
            dump_expr(out, &for_stmt.start, depth + 2);
            indent(out, depth + 1);
            out.push_str("End\n");
            dump_expr(out, &for_stmt.end, depth + 2);
            if let Some(step) = &for_stmt.step {
                indent(out, depth + 1);
                out.push_str("Step\n");
                dump_expr(out, step, depth + 2);
            }
            indent(out, depth + 1);
            out.push_str("Body\n");
            for s in &for_stmt.body {
                dump_stmt(out, s, depth + 2);
            }
        }
        Stmt::ForIn(for_in) => {
            indent(out, depth);
            match &for_in.index {
                Some(index) => {
                    let _ = writeln!(
                        out,
                        "ForIn [`{}`, `{}`] @ {}",
                        index.name,
                        for_in.element.name,
                        fmt_span(for_in.span)
                    );
                }
                None => {
                    let _ = writeln!(
                        out,
                        "ForIn `{}` @ {}",
                        for_in.element.name,
                        fmt_span(for_in.span)
                    );
                }
            }
            indent(out, depth + 1);
            out.push_str("Iterable\n");
            dump_expr(out, &for_in.iterable, depth + 2);
            indent(out, depth + 1);
            out.push_str("Body\n");
            for s in &for_in.body {
                dump_stmt(out, s, depth + 2);
            }
        }
        Stmt::While(while_stmt) => {
            indent(out, depth);
            let _ = writeln!(out, "While @ {}", fmt_span(while_stmt.span));
            indent(out, depth + 1);
            out.push_str("Cond\n");
            dump_expr(out, &while_stmt.cond, depth + 2);
            indent(out, depth + 1);
            out.push_str("Body\n");
            for s in &while_stmt.body {
                dump_stmt(out, s, depth + 2);
            }
        }
        Stmt::Switch(switch) => {
            dump_switch(out, switch, "SwitchStmt", depth);
        }
        Stmt::Break { span } => {
            indent(out, depth);
            let _ = writeln!(out, "Break @ {}", fmt_span(*span));
        }
        Stmt::Continue { span } => {
            indent(out, depth);
            let _ = writeln!(out, "Continue @ {}", fmt_span(*span));
        }
    }
}

/// Render an if-expression's/if-statement's branch list (a `Vec<Stmt>`) with
/// an optional else branch, used by both the if-expression dump and the
/// switch arm dump.
fn dump_if_expr(out: &mut String, if_expr: &IfExpr, label: &str, depth: usize) {
    indent(out, depth);
    let _ = writeln!(out, "{label} @ {}", fmt_span(if_expr.span));
    indent(out, depth + 1);
    out.push_str("Cond\n");
    dump_expr(out, &if_expr.cond, depth + 2);
    indent(out, depth + 1);
    out.push_str("Then\n");
    for s in &if_expr.then_branch {
        dump_stmt(out, s, depth + 2);
    }
    if let Some(else_branch) = &if_expr.else_branch {
        indent(out, depth + 1);
        out.push_str("Else\n");
        for s in else_branch {
            dump_stmt(out, s, depth + 2);
        }
    }
}

/// Render a `switch` (statement or expression form).
fn dump_switch(out: &mut String, switch: &SwitchStmt, label: &str, depth: usize) {
    indent(out, depth);
    let _ = writeln!(out, "{label} @ {}", fmt_span(switch.span));
    if let Some(subject) = &switch.subject {
        indent(out, depth + 1);
        out.push_str("Subject\n");
        dump_expr(out, subject, depth + 2);
    }
    for arm in &switch.arms {
        indent(out, depth + 1);
        match &arm.label {
            Some(label) => {
                let _ = writeln!(out, "Arm @ {}", fmt_span(arm.span));
                indent(out, depth + 2);
                out.push_str("Label\n");
                dump_expr(out, label, depth + 3);
            }
            None => {
                let _ = writeln!(out, "Default @ {}", fmt_span(arm.span));
            }
        }
        indent(out, depth + 2);
        out.push_str("Body\n");
        for s in &arm.body {
            dump_stmt(out, s, depth + 3);
        }
    }
}

/// Render one call argument. A named argument prints its name on its own
/// line, then the value indented under it; a positional argument prints the
/// value directly.
fn dump_call_arg(out: &mut String, arg: &CallArg, depth: usize) {
    match &arg.name {
        Some(name) => {
            indent(out, depth);
            let _ = writeln!(out, "NamedArg `{}` @ {}", name.name, fmt_span(arg.span));
            dump_expr(out, &arg.value, depth + 1);
        }
        None => dump_expr(out, &arg.value, depth),
    }
}

fn dump_expr(out: &mut String, expr: &Expr, depth: usize) {
    // The compound value forms print their own header line via shared
    // helpers; every other form prints a single leading-indented line here.
    match expr {
        Expr::If(if_expr) => {
            dump_if_expr(out, if_expr, "IfExpr", depth);
            return;
        }
        Expr::Switch(switch) => {
            dump_switch(out, switch, "SwitchExpr", depth);
            return;
        }
        Expr::Loop(stmt) => {
            indent(out, depth);
            let _ = writeln!(out, "LoopExpr @ {}", fmt_span(stmt.span()));
            dump_stmt(out, stmt, depth + 1);
            return;
        }
        _ => {}
    }
    indent(out, depth);
    match expr {
        Expr::Int { value, span } => {
            let _ = writeln!(out, "Number {value} (int) @ {}", fmt_span(*span));
        }
        Expr::Float { value, span } => {
            let _ = writeln!(out, "Number {value} (float) @ {}", fmt_span(*span));
        }
        Expr::Bool { value, span } => {
            let _ = writeln!(out, "Bool {value} @ {}", fmt_span(*span));
        }
        Expr::Na { span } => {
            let _ = writeln!(out, "Na @ {}", fmt_span(*span));
        }
        Expr::Str { value, span } => {
            let _ = writeln!(out, "Str {value:?} @ {}", fmt_span(*span));
        }
        Expr::Color { r, g, b, t, span } => {
            if *t == 0.0 {
                let _ = writeln!(out, "Color #{r:02X}{g:02X}{b:02X} @ {}", fmt_span(*span));
            } else {
                let _ = writeln!(
                    out,
                    "Color #{r:02X}{g:02X}{b:02X} t={t} @ {}",
                    fmt_span(*span)
                );
            }
        }
        Expr::Ident(ident) => {
            let _ = writeln!(out, "Ident `{}` @ {}", ident.name, fmt_span(ident.span));
        }
        Expr::Unary { op, operand, span } => {
            let _ = writeln!(out, "Unary `{}` @ {}", op.symbol(), fmt_span(*span));
            dump_expr(out, operand, depth + 1);
        }
        Expr::Binary { op, lhs, rhs, span } => {
            let _ = writeln!(out, "Binary `{}` @ {}", op.symbol(), fmt_span(*span));
            dump_expr(out, lhs, depth + 1);
            dump_expr(out, rhs, depth + 1);
        }
        Expr::Ternary {
            cond,
            then_e,
            else_e,
            span,
        } => {
            let _ = writeln!(out, "Ternary @ {}", fmt_span(*span));
            dump_expr(out, cond, depth + 1);
            dump_expr(out, then_e, depth + 1);
            dump_expr(out, else_e, depth + 1);
        }
        Expr::Call {
            callee,
            args,
            type_args,
            span,
        } => {
            let _ = writeln!(out, "Call `{}` @ {}", callee.name, fmt_span(*span));
            for ta in type_args {
                indent(out, depth + 1);
                let _ = writeln!(out, "TypeArg `{}` @ {}", ta.name, fmt_span(ta.span));
            }
            for arg in args {
                dump_call_arg(out, arg, depth + 1);
            }
        }
        Expr::MethodCall {
            receiver,
            method,
            args,
            span,
        } => {
            let _ = writeln!(out, "MethodCall `{}` @ {}", method.name, fmt_span(*span));
            dump_expr(out, receiver, depth + 1);
            for arg in args {
                dump_call_arg(out, arg, depth + 1);
            }
        }
        Expr::FieldAccess {
            receiver,
            field,
            span,
        } => {
            let _ = writeln!(out, "FieldAccess .{} @ {}", field.name, fmt_span(*span));
            dump_expr(out, receiver, depth + 1);
        }
        Expr::History {
            target,
            offset,
            span,
        } => {
            let _ = writeln!(out, "History @ {}", fmt_span(*span));
            dump_expr(out, target, depth + 1);
            dump_expr(out, offset, depth + 1);
        }
        Expr::Group { inner, span } => {
            let _ = writeln!(out, "Group @ {}", fmt_span(*span));
            dump_expr(out, inner, depth + 1);
        }
        Expr::Tuple { elements, span } => {
            let _ = writeln!(out, "Tuple @ {}", fmt_span(*span));
            for element in elements {
                dump_expr(out, element, depth + 1);
            }
        }
        // Handled before the leading indent above.
        Expr::If(_) | Expr::Switch(_) | Expr::Loop(_) => unreachable!(),
    }
}

/// Keep whole transparency percentages as integer JSON numbers for stored
/// programs, while retaining every finite fractional percentage verbatim.
pub fn serialize_transparency<S: serde::Serializer>(
    value: &f64,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    if value.is_finite() && (0.0..=100.0).contains(value) && value.fract() == 0.0 {
        serializer.serialize_u8(*value as u8)
    } else {
        serializer.serialize_f64(*value)
    }
}
