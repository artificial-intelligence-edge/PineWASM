# Pine Script® libraries

PineWASM has first-class support for Pine v6 libraries: a script may
`import Publisher/LibName/Version as alias` and call `alias.fn(...)`, and the
exported function is linked into the program and inlined per call site exactly
like a local function. The compiler resolves imports **against a caller-supplied
source map only, never the network** (`compile_with_libraries`); the host is
what gathers library source and threads it in.

Libraries are a *language* feature. Nothing in this repository downloads,
publishes, or brokers Pine source: a host hands the compiler a source map, and
the compiler links what it was given.

## Host integration

A host compiles a script and its libraries as one multi-source unit by passing
an array of `[importPath, source]` pairs to `compileWithLibraries` or
`compileAndRunWithLibraries`. Nothing is resolved beyond that explicit map.

The JavaScript exports validate and snapshot the supplied library map before
compilation. See the [JavaScript library contract](browser.md#libraries) for
the accepted data shape, string validation, size limits and host prerequisites.

Every library is parsed under its own `SourceId`. `SourceSpan.source` is `0`
for the main script and `i + 1` for `libraries[i]`, so a host can route each
diagnostic to the correct source buffer. An import with no matching map entry
fails with `E0243`.

Library source may carry `// @function`, `// @param`, and
`// @returns` annotations above an `export`. An embedding application may use
those comments for documentation or editor assistance.

Caller-supplied library source is not part of PineWASM and is not covered by
PineWASM's license merely because the compiler can process it. A host or
distributor is responsible for confirming that it has the rights needed to use
or redistribute that source.

## Compiler linking

Linking adds **no new CSIL opcodes or runtime concepts** — PineWASM implements
it through per-call-site inlining (see [architecture](architecture.md) and
[intermediate representations](ir.md)).
Details of `export`/`import`/namespace isolation are in
[feature coverage](pine-v6-coverage.md) and [language support](language-support.md).

### Imported object types and enums

Exported types and enums are registered under the library's own identity and
exposed through its import alias. Constructors, typed arrays, and methods can
therefore refer to a library-defined object without losing its type during
linking. Private types remain usable inside the library but are not public
alias members. Two libraries declaring the same type name remain distinct.
The synthetic library tests cover object fields and methods, private names,
and enum/type identity across aliases; source resolution still uses the
caller-provided map and makes no network request.

### Top-level statement pruning

Library source can contain top-level statements in addition to exported
definitions. When linking an import, `resolve_library` keeps only the library
globals read by exported functions and their private dependencies. It drops
unrelated globals and every bare top-level statement, so top-level output from
the library is not injected into the importing script and an unused library
global cannot collide with an importer variable of the same name.

## Library-oriented syntax

Library linking exercises a wide slice of the language surface. The following
forms are accepted in library source and in scripts that import it:

| Feature | Notes |
|---|---|
| Single-quoted strings (`'x'`) | Interchangeable with `"x"`; the other quote is an ordinary character inside. |
| Trailing-dot floats (`8.`) | Equal to `8.0`; the dot stays separate before an identifier (`x.field`). |
| Exponent literals (`10e10`, `2.5e-3`) | Float-typed. |
| Line wrapping | A continuation line indented by a non-block amount joins the previous logical line. |
| Typed / qualified parameters | `f(series float src, simple int n, array<color> shades)` — the qualifier is accepted and dropped; the type is recorded. A parameter literally named after a qualifier still works. |
| `array<T>` of any element / `T[]` sugar | `array<color>`, `int[]`, `MyType[]`. |
| UDT / enum variable annotations | `Settings s = Settings.new(...)`, beyond the drawing types. |
| Compound assignment | `+= -= *= /= %=` desugar to `x := x op e`. |
| `input.*` UI metadata | `tooltip`, `inline`, `group`, `confirm`, and `options` (string inputs) — dialog-shaping only, no data-path effect. |
| Negative input defaults | `input.float(defval=-0.1)`, `input.int(-5)`. |
| Numeric `plotshape`/`plotchar` data | Accept `series int/float` as well as `series bool`; PineWASM plots a shape wherever the value is not `na` and not `0` (the `cond ? price : na` idiom). |

Two propagation rules let the compiler accept common length and parameter shapes:

- **Compile-time int lengths** (`Symbol::const_int`). A `ta.*` length is a
  `simple int` in v6 — a literal, a const-valued variable, or an `input.int(...)`
  (fixed for the run) all qualify. The value flows into the engine call through
  inlined parameters, so `lib.smooth(close, 14)` and `lib.smooth(close, input.int(14))`
  both link. The compile-time engine window is baked from that value (an input
  uses its default; a reassignment clears the stamp). **Limitation:** a host
  input-*override* does not resize the (compile-time) window — PineWASM has no
  dynamic engine windows.
- **History on a parameter.** A function parameter is a real per-bar slot, so
  `param[i]` indexes the values stored into it each bar — valid even when the
  argument is `simple`-qualified, and for a UDT field that holds a series
  (`kernels.rationalQuadratic(settings.source, …)` where the body does
  `_src[i]`). Non-slot targets (const literals, enum members, drawing `.all`)
  still require a series.

The linked tests in [`pine-v6-coverage.md`](pine-v6-coverage.md) exercise these
rules with tracked test fixtures and in-memory source maps.
