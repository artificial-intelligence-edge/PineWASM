//! Tokens produced by the lexer.

use pinewasm_span::Span;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TokenKind {
    // Literals and names. The token stores no text; consumers slice the
    // source by span.
    Ident,
    Number,
    /// A double-quoted string literal (span includes the quotes).
    Str,
    /// A hex color literal `#RRGGBB` or `#RRGGBBAA` (span includes the `#`).
    Color,

    // Keywords.
    KwIf,
    KwElse,
    KwTrue,
    KwFalse,
    KwAnd,
    KwOr,
    KwNot,
    KwVar,
    /// `varip` — intrabar-persistent declaration. Lowers like `var`
    /// (persistent slot, run-once init) but additionally survives
    /// realtime-tick rollback. A keyword, so it can never be an identifier.
    KwVarip,
    /// `int` — type annotation keyword (`int x = 1`).
    KwInt,
    /// `float` — type annotation keyword (`float x = 1.5`).
    KwFloat,
    /// `bool` — type annotation keyword (`bool ok = true`).
    KwBool,
    /// `string` — type annotation keyword (`string s = "a"`).
    KwString,
    /// `na` — the not-available literal (also the callee of the `na(x)`
    /// builtin; the parser decides by the following token).
    KwNa,
    /// `for` — opens a counted loop (`for i = start to end [by step]`).
    KwFor,
    /// `to` — separates the start and end bounds of a `for` loop.
    KwTo,
    /// `by` — introduces the optional step of a `for` loop.
    KwBy,
    /// `in` — separates the loop variable(s) from the iterable in a `for…in`
    /// loop (`for element in arr`).
    KwIn,
    /// `while` — opens a condition-driven loop (`while cond`).
    KwWhile,
    /// `break` — exit the innermost loop (valid only inside a loop body).
    KwBreak,
    /// `continue` — skip to the next iteration of the innermost loop (valid
    /// only inside a loop body).
    KwContinue,
    /// `switch` — opens a switch statement or expression.
    KwSwitch,
    /// `enum` — opens an enum declaration (`enum Name` + an indented member
    /// list). Members are referenced as `Name.member` and form a distinct
    /// value type.
    KwEnum,
    /// `type` — opens a user-defined type declaration (`type Name` + an
    /// indented field list). Fields form a record; instances are constructed
    /// with `Name.new(...)` and their fields read/written via
    /// `obj.field`.
    KwType,
    /// `method` — opens a user-defined method declaration
    /// (`method f(TypeName this, ...) => body`). The first parameter's declared
    /// type is the receiver; `obj.f(args)` resolves to this method.
    KwMethod,

    // Punctuation and operators.
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    /// `+=` — compound reassignment (desugars to `x := x + e`).
    PlusEq,
    /// `-=` — compound reassignment.
    MinusEq,
    /// `*=` — compound reassignment.
    StarEq,
    /// `/=` — compound reassignment.
    SlashEq,
    /// `%=` — compound reassignment.
    PercentEq,
    Lt,
    Le,
    Gt,
    Ge,
    EqEq,
    BangEq,
    /// `=` (declaration/assignment).
    Eq,
    /// `:=` (reassignment).
    ColonEq,
    /// `=>` — introduces a `switch` arm body.
    FatArrow,
    /// `?` — opens a ternary conditional (`cond ? a : b`).
    Question,
    /// `:` — separates the branches of a ternary conditional. Only valid
    /// there; a stray `:` anywhere else is a parse error (`:=` lexes as one
    /// [`TokenKind::ColonEq`] token, never as `:` `=`).
    Colon,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    /// `.` — joins namespaced built-in names (`ta.sma`). The number rule
    /// consumes fractional dots itself, so this token only appears where a
    /// number literal does not claim the dot.
    Dot,

    /// A `//@version=N` directive comment. The span covers the whole
    /// directive; the parser re-lexes the number from the source text.
    VersionDirective,

    /// A `//@strategy_alert_message=...` compiler-annotation comment. The span
    /// covers the whole line; the parser reads the message text from the source.
    /// Stored as strategy metadata (like `//@version`); the rest-of-line is a
    /// free-form string, so it is not tokenized further.
    AlertMessageDirective,

    /// End of a logical line.
    Newline,
    /// Increase of indentation depth (block start).
    Indent,
    /// Decrease of indentation depth (block end).
    Dedent,
    /// End of input. Always the final token.
    Eof,

    /// A character sequence the lexer could not tokenize. A diagnostic is
    /// emitted alongside; the token lets the parser resynchronize.
    Error,
}

// Keep the lexer lookup and the public tooling inventory generated from one
// table. Language-tooling clients consume `TokenKind::KEYWORDS`; adding a
// keyword here therefore changes parsing and completion atomically.
macro_rules! define_keywords {
    ($($text:literal => $variant:ident),+ $(,)?) => {
        /// Every reserved source keyword and its token kind.
        pub const KEYWORDS: &'static [(&'static str, TokenKind)] = &[
            $(($text, TokenKind::$variant)),+
        ];

        pub fn keyword_from_str(s: &str) -> Option<TokenKind> {
            Some(match s {
                $($text => TokenKind::$variant,)+
                _ => return None,
            })
        }
    };
}

impl TokenKind {
    /// Human-readable name used in "expected X, found Y" diagnostics.
    pub fn describe(self) -> &'static str {
        match self {
            TokenKind::Ident => "identifier",
            TokenKind::Number => "number",
            TokenKind::Str => "string",
            TokenKind::Color => "color literal",
            TokenKind::KwIf => "`if`",
            TokenKind::KwElse => "`else`",
            TokenKind::KwTrue => "`true`",
            TokenKind::KwFalse => "`false`",
            TokenKind::KwAnd => "`and`",
            TokenKind::KwOr => "`or`",
            TokenKind::KwNot => "`not`",
            TokenKind::KwVar => "`var`",
            TokenKind::KwVarip => "`varip`",
            TokenKind::KwInt => "`int`",
            TokenKind::KwFloat => "`float`",
            TokenKind::KwBool => "`bool`",
            TokenKind::KwString => "`string`",
            TokenKind::KwNa => "`na`",
            TokenKind::KwFor => "`for`",
            TokenKind::KwTo => "`to`",
            TokenKind::KwBy => "`by`",
            TokenKind::KwIn => "`in`",
            TokenKind::KwWhile => "`while`",
            TokenKind::KwBreak => "`break`",
            TokenKind::KwContinue => "`continue`",
            TokenKind::KwSwitch => "`switch`",
            TokenKind::KwEnum => "`enum`",
            TokenKind::KwType => "`type`",
            TokenKind::KwMethod => "`method`",
            TokenKind::Plus => "`+`",
            TokenKind::Minus => "`-`",
            TokenKind::Star => "`*`",
            TokenKind::Slash => "`/`",
            TokenKind::Percent => "`%`",
            TokenKind::PlusEq => "`+=`",
            TokenKind::MinusEq => "`-=`",
            TokenKind::StarEq => "`*=`",
            TokenKind::SlashEq => "`/=`",
            TokenKind::PercentEq => "`%=`",
            TokenKind::Lt => "`<`",
            TokenKind::Le => "`<=`",
            TokenKind::Gt => "`>`",
            TokenKind::Ge => "`>=`",
            TokenKind::EqEq => "`==`",
            TokenKind::BangEq => "`!=`",
            TokenKind::Eq => "`=`",
            TokenKind::ColonEq => "`:=`",
            TokenKind::FatArrow => "`=>`",
            TokenKind::Question => "`?`",
            TokenKind::Colon => "`:`",
            TokenKind::LParen => "`(`",
            TokenKind::RParen => "`)`",
            TokenKind::LBracket => "`[`",
            TokenKind::RBracket => "`]`",
            TokenKind::Comma => "`,`",
            TokenKind::Dot => "`.`",
            TokenKind::VersionDirective => "version directive",
            TokenKind::AlertMessageDirective => "strategy alert message directive",
            TokenKind::Newline => "end of line",
            TokenKind::Indent => "indented block",
            TokenKind::Dedent => "end of block",
            TokenKind::Eof => "end of input",
            TokenKind::Error => "invalid token",
        }
    }

    define_keywords! {
        "if" => KwIf,
        "else" => KwElse,
        "true" => KwTrue,
        "false" => KwFalse,
        "and" => KwAnd,
        "or" => KwOr,
        "not" => KwNot,
        "var" => KwVar,
        "varip" => KwVarip,
        "int" => KwInt,
        "float" => KwFloat,
        "bool" => KwBool,
        "string" => KwString,
        "na" => KwNa,
        "for" => KwFor,
        "to" => KwTo,
        "by" => KwBy,
        "in" => KwIn,
        "while" => KwWhile,
        "break" => KwBreak,
        "continue" => KwContinue,
        "switch" => KwSwitch,
        "enum" => KwEnum,
        "type" => KwType,
        "method" => KwMethod,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Token { kind, span }
    }

    /// The source text this token covers.
    pub fn text<'s>(&self, source: &'s str) -> &'s str {
        source
            .get(self.span.start as usize..self.span.end as usize)
            .unwrap_or("")
    }
}
