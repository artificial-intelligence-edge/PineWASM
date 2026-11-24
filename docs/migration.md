# Migrating Pine v5 source to v6

PineWASM compiles Pine Script® v6. A `//@version=5` script is rejected with
`E0249` rather than compiled as v6, because the two versions disagree about
what some unchanged-looking code computes:

- `5 / 2` is `2` in v5 when both operands are `const`, and `2.5` in v6.
- `and` and `or` evaluate both operands in v5; v6 skips the right operand
  when the left one decides the result, so a `ta.*` call there stops
  updating on some bars.
- A v5 `bool` can be `na`; a v6 `bool` cannot.
- `timeframe.period` is `"D"` in v5 and `"1D"` in v6.
- `color.red`, `color.teal`, and `color.yellow` have different shades.

Compiling v5 source as v6 would therefore change results with no error.
`pinewasm migrate` rewrites a v5 script as v6 source that compiles. Wherever
v6 can express the original behavior, each rewrite carries a class (exact,
exactWithHoisting, approximate or diagnostic) that states how closely it keeps
the v5 behavior.

## Using it

`pinewasm` is the command-line binary. `cargo xtask build` builds it at
`target/debug/pinewasm`; run it from there or put that folder on your `PATH`.

```sh
pinewasm migrate script.pine            # print the v6 source
pinewasm migrate script.pine --write    # rewrite the file in place
pinewasm migrate script.pine --json     # status, output, edits, findings
```

Options:

| Option | Effect |
| --- | --- |
| `--idiomatic` | Prefer the conventional v6 spelling where it differs from an exact reproduction of v5 (see [Profiles](#profiles)). |
| `--modernize` | Also replace older spellings (`float[]` → `array<float>`, `array.new_float()` → `array.new<float>()`). These change nothing at runtime. |
| `--strict-indices` | Guard known-negative literal indices and indices that come from a search (`array.indexof`), so a negative index raises a runtime error as it did in v5 instead of reading from the end of the array. |

Findings are printed to standard error as
`file:line: note|warning|review [rule] message`. A `note` records a rewrite
that needs no action. The command exits nonzero only when the file does not
parse. A script that is already v6 is printed unchanged and the command exits
zero.

In the language server, a v5 document shows the `E0249` error plus the sites
that need review. Its code actions offer **Convert script to Pine v6** for
the whole file, and one **Migrate to v6** quick fix per rewrite under the
cursor. A single quick fix keeps `//@version=5`: each rewrite is valid v5
on its own, so the script keeps compiling as v5 until it is converted in full.

The WebAssembly package exposes the same function as `migrateToV6(source,
idiomatic)` in builds with the `migrate` feature of `pinewasm_bindings`.

## What it guarantees

Every rewrite belongs to one of four classes, reported with each finding:

| Class | Meaning |
| --- | --- |
| `exact` | A local rewrite. The v6 output keeps the v5 meaning of the input. |
| `exactWithHoisting` | Exact, but it adds temporaries or restructures a statement (see [Evaluation order](#evaluation-order)). |
| `approximate` | Compiles and keeps the v5 behavior except in stated edge cases. |
| `diagnostic` | v6 has no spelling for the v5 behavior. The site is reported for review, and the output may compute something different there. |

The rewrite splices edits into the original text instead of reprinting the
script, so comments, blank lines, and formatting outside the rewritten
expressions are preserved byte for byte.

## Rules

Rule identifiers match the migration specification the tool implements.

### Calls and strategies

| Rule | v5 construct | v6 output | Class |
| --- | --- | --- | --- |
| B2 | The same parameter passed twice (v5 used the first) | The repeats are removed | exact |
| B3 | Positional arguments whose v6 position means something else (for example, `f(a, b)` where v6 gives the second position a different meaning) | Those arguments are named | exact |
| C1 | `when = cond` on `strategy.*` orders, named or positional | `if cond` around the call; arguments with history or effects are evaluated first, because v5 evaluated them on every bar | exact / with hoisting |
| C2 | `strategy.exit` given both `limit` and `profit` (or `stop`/`loss`, `trail_price`/`trail_points`) | The relative level applies only where the absolute one is `na`, which is how v5 chose between them | exact / with hoisting |
| C3 | A strategy without `margin_long`/`margin_short` | `margin_long = 0, margin_short = 0` | exact |
| D4a | `na` passed where a special constant is required (`plot(style = na)`) | The argument is removed or given its default constant | exact |
| D8 | `linewidth` below 1 (v5 raised it to 1) | `math.max(1, width)`, including literals | exact |
| G3 | `label.new` without `textcolor` | `textcolor = color.black` | exact |
| G4 | `transp = t` | Folded into the color with `color.new()` (both explicit opaque arrow colors; in a conditional color, each plain color branch, leaving `na` branches as they are), or removed where v5 ignored it | exact / approximate |

### Types and `na`

| Rule | v5 construct | v6 output | Class |
| --- | --- | --- | --- |
| D1 | A number used as a condition or compared with a `bool` using `==`/`!=` | `bool(x)`, which treats `0` and `na` as false | exact |
| D2 | `na` where a `bool` is expected | `false` | exact |
| D2g | `nz(b)` / `fixnan(b)` on a `bool` | `b` | exact |
| D2h | `na(a < b)` | `na(a) or na(b)` | exact |
| D2i | `na(b[n])` where `b` is a global `bool` that is never `na` in v5 (a literal, a `barstate.*` or `session.is*` value, or a `ta.cross*` result) | `bar_index < n`, true exactly while the history does not exist yet | exact |
| D3 | `x == na` (never true in v5) | `false`; the idiomatic profile writes `na(x)` | exact / approximate |
| D4b, D4c | A conditional of a special constant type that can yield `na`, as an expression or as the final statement of a function body | The `na` becomes the parameter's default, or an `else` returning it is added. For an `hline` style, whose default is undocumented, the `na` or the added branch becomes `hline.style_solid`; that matters only when that branch is selected | exact / approximate |
| D5 | `a / b` where both are `const int` in v5 | `int(a / b)` | exact |
| D6 | A reassigned variable passed where a constant is required | The value it had on the first bar | exact |
| D9 | A value-producing function, conditional block or loop body ends in a variable declaration or reassignment, or a statement `if` branch ends in a declaration | An explicit read of that variable follows the assignment; the read changes no value | exact for multiline blocks |

D5 and D6 depend on which values v5 considered `const`. The model follows
v5's rules, which v6 changed. The migrator treats a variable reassigned only at its own
block level, and only with constants, as `const`, so the converter
wraps `b / 2` in `int()` for the sequence `b = 7`, `b := 8`. A reassigned
`var` becomes `simple`, so its divisions keep their fraction. An unqualified typed function parameter such
as `f(int n)` is at least `simple` even when called with a literal; an
untyped parameter `f(n)` can inherit `const`, and an explicit `const int n`
stays `const`. The exact profile retains these distinctions when deciding
where to add `int()`.

### Evaluation order

| Rule | v5 construct | v6 output | Class |
| --- | --- | --- | --- |
| E1 | A right operand of `and`/`or` with history or effects | The operand is evaluated into a temporary before the statement | with hoisting |
| E2 | A counted `for` without an explicit step | Append `by 1` to the loop header, under the explicit-step migration rule | exact source rewrite |
| E3 | A compound reassignment such as `x += rhs` | Expand to `x := x + rhs`, grouping the right side when needed to preserve evaluation order | exact source rewrite |
| E4 | A `for` end bound that the loop body can change | The bound is computed once into a temporary before the loop and the loop uses it as its end | with hoisting |

Under the v5 rule, a counted loop's end bound is evaluated once, on loop
entry; v6 re-evaluates it before every iteration. When the body can change
the bound (it reassigns a variable the bound reads, calls a mutating built-in
on a collection the bound reads, calls a user function or method, or the bound
itself has history or effects), E4 saves the entry value so the migrated loop
runs the same iterations as v5. For example, `for i = 0 to array.size(a) - 1`
with `array.pop(a)` in the body has its bound saved at loop entry. A bound the
body cannot change (a constant, an input, arithmetic over variables that are
never reassigned, or a collection the body only reads) stays inline. The same applies to a `for` loop used as a
value (`x = for ...`).

Migration leaves the step expression intact. A loop whose evaluated `by`
step is nonpositive therefore still stops with a runtime error after
conversion, instead of running with the step's absolute value.
The unit step is a positive magnitude, so an originally descending loop still
counts downward.

Temporaries are named `__v6_1`, `__v6_2`, … and never collide with names in
the script. Wherever an expression is lazily evaluated, the temporary goes
where v5 evaluated it, not simply above the statement:

- In a ternary branch, the ternary becomes an `if` expression and the
  temporary goes inside the branch.
- In an `else if` condition, the chain gains an `else` block holding the
  temporary and the nested `if`.
- In a `while` condition, the loop becomes `while true` with the
  condition, and its temporaries, at the top of the body.
- In a later `switch` case, the `switch` becomes an `if` chain.
- Inside a `request.security()` expression, the expression moves into a
  new global function, so the temporary is evaluated in the requested
  context.
- In a `var` initializer, the temporary is itself `var`.

### History, values, and requests

| Rule | v5 construct | v6 output | Class |
| --- | --- | --- | --- |
| F1 | `[]` on a literal or built-in constant | The constant is stored in a temporary first, keeping the leading `na` on the first bars | with hoisting |
| F2 | `obj.field[n]` | The object's history, then the field. An object that is persistent or modified is snapshotted instead and reported. | exact / approximate |
| G1 | `timeframe.period` | Comparisons with `"D"`, `"W"`, `"M"`, `"S"` gain the `1`. Uses that can observe the text call a helper returning the v5 spelling. Timeframe arguments are left alone. The compiler recognizes the exact helper result as the current chart timeframe when it selects a request context. | exact |
| G2 | `color.red`, `color.teal`, `color.yellow` | Their v5 hex values (the idiomatic profile keeps the names) | exact |
| H1 | A script using `request.*()` | Leave an omitted `dynamic_requests` argument omitted by default. The exact profile pins `false` only for the globally seeded nullary-wrapper case, which needs full-chart request values. Remove an explicit `false` when the source requires local/conditional request execution. | exact / diagnostic |

### Review findings

These are reported and not rewritten, because v6 cannot express the v5
behavior:

| Finding | Situation |
| --- | --- |
| D-1 | Code that distinguishes a `bool` `na` from `false`; tuple branches mixing explicit `na` and `bool` require manual conversion. |
| D-2 | A `series` plot `offset` needs review, because v6 cannot express the plot as it is written. |
| D-3 | A reassigned variable where v6 requires a constant, and its first-bar value cannot be computed. |
| D-4 | A request nested in another request's expression, in a script that needs dynamic requests. |
| D-5 | `timeframe.period` text under the idiomatic profile. |
| D-7 | A negative `trail_points` (activation needs review). |
| D-8 | `time()` with `bars_back` on another timeframe (the bars are counted on a different timeframe). |
| D-11 | A known-negative literal or an array index from a search, which can be `-1` (a negative index reads from the end in v6; `--strict-indices` adds a guard). |
| D-12 | Colors built from input colors, which hide the input's picker. |
| D-13 | `na` for a strategy direction or a script's `scale`, which have no default constant. |

A nested request through a nullary wrapper can change which symbol's values
are requested after conversion. Calling that wrapper from a direct global
assignment as well as a local block is the narrow case the converter recognizes: it pins
`dynamic_requests = false` under rule H1, and other nested requests still
receive D-4 for review. PineWASM's checker currently reports E0244 for
this compound request expression.

## Profiles

The default **exact** profile reproduces v5 results wherever v6 can. The
**idiomatic** profile differs in four places, where the exact spelling is
unusual or where the v5 code almost certainly meant the v6 behavior:

| Construct | Exact | Idiomatic |
| --- | --- | --- |
| `x == na` | `false` (its v5 value) | `na(x)` |
| `color.red` | `#FF5252` | `color.red` (the v6 shade) |
| `6[1]` | A temporary, keeping the leading `na` | `6` |
| `timeframe.period` shown as text | A helper returning `"D"` | Unchanged, reported as D-5 |

## Limits

- The migrator reads source through PineWASM's parser. Scripts using syntax
  the parser does not accept yet are reported as `parseError` and left
  unchanged; the largest remaining gap is several statements on one line
  separated by commas. The parser's input limits apply as well: a script over
  1 MiB is `parseError`, and so is one whose v6 text would pass that limit,
  because the migrator returns a whole migration or none.
- Built-in type words such as `int`, `float`, `bool`, `string`, and `type` are
  treated as identifiers where Pine permits names, while declaration and cast
  positions keep their type meaning.
- When declarations with the same receiver name have conflicting types, the
  converter keeps the type unknown and conservatively preserves evaluation.
  It does not infer a receiver type from hash-map iteration order.
- Version 4 and earlier scripts must first be brought to v5.
- The output is valid v6, but it can use constructs PineWASM does not yet
  compile (for example, stateful calls inside loop bodies). `pinewasm check`
  on the output reports those as ordinary compiler diagnostics.

## Repeatable validation

Run the first-party suite with:

```sh
cargo test --locked -p pinewasm_migrate
cargo test --locked -p pinewasm_migrate --test adversarial -- --ignored
```

The layers check different contracts:

| Suite | Contract |
| --- | --- |
| `golden.rs` | Purpose-written v5/v6 pairs for individual rules, exact text, edit replay, idempotence, and compilation where supported. |
| `rules.rs` | For every rule: negative cases the rule must leave alone, the rule inside functions, methods, conditionals, loops, requests and library exports, and the exact source span of each review finding. It also checks the [coverage table](#test-coverage) below against the test files and the rules the migrator reports. |
| `scripts.rs` | Twenty full-length purpose-written scripts in `tests/scripts/` (indicators, strategies, a library) that each exercise many rules at once: exact v6 text, a findings snapshot (rule and line), edit replay, idempotence, parse validity, and compilation where PineWASM supports every construct. |
| `findings.rs` | Review-required cases, option behavior, source locations, partial edits, and unsupported versions. |
| `composition.rs` | Complete strategy conversion; all eight profile/modernize/strict-index combinations; Unicode and CRLF preservation; temporary-name collisions; search aliases and shadowing. |
| `behavior.rs` | Hand-derived expected values over synthetic bars: 42 signed division combinations, qualifiers, nested division, eager stateful operands, missing history, conditions, loop bounds that change in the body, a complete indicator, and strict-index runtime errors. |
| `full_script_parity.rs` | Complete purpose-written v5 fixtures in `tests/regression/migration-parity-*`: exact match with each reviewed `-v6.pine` migration, idempotence, and runtime plot traces derived by hand from v5 semantics (eager operands, first-bar history, const division, and a loop whose body shrinks its end bound). |
| `adversarial.rs` | Reproducible generated and mutated source; totality, edit replay, valid parsed output and idempotence. This is explicitly invoked by the existing fuzz-smoke gate. |

Coverage follows the behavior changes that the migrator implements: numeric conditions,
boolean missing values, lazy operators, requests, integer division, strategy
orders/margins/exit precedence, history, repeated parameters, offsets, unique
types, timeframe spelling, negative indices, mutable qualifiers, transparency,
colors, and changing loop bounds. Strategy order trimming and unrepresentable
v5 behavior remain review concerns; passing these tests is not a proof that
every possible script or backtested strategy trade is equivalent.

The strict-index search detector follows direct searches and immutable aliases,
including scoped names. It is not a general data-flow proof through assignments,
function parameters, or arbitrary arithmetic. The default profile reports a
review finding for detected searches; `--strict-indices` inserts the guard.

### Test coverage

Each rule the migrator reports has at least one test of the rewrite (or,
for a review finding, of the finding and its span) and one test showing
what it leaves alone. `rules.rs` checks that every test named here exists
and that every rule identifier the migrator can report has a row, so a new
rule without tests fails the suite. The full-script tier exercises most
rules together; each script's `.findings` file lists which.

| Rule | Rewrite or finding | Left alone |
| --- | --- | --- |
| A1 | `golden::version_directive_becomes_6` | `findings::statuses_for_inputs_that_are_not_migrated` |
| B2 | `golden::duplicate_argument_keeps_the_first`, `rules::b2_repeat_inside_a_function_keeps_the_first`, `rules::b2_positional_then_named_title_keeps_the_positional` | `rules::b2_distinct_arguments_are_untouched` |
| B3 | `golden::input_source_confirm_is_named`, `golden::time_bars_back_is_named`, `rules::b3_time_bars_back_in_ternary_branches_on_the_chart_timeframe` | `rules::b3_short_positional_lists_and_named_arguments_are_untouched` |
| C1 | `golden::when_moves_into_if_and_evaluates_stateful_arguments_first`, `golden::positional_when_on_strategy_close`, `golden::when_true_is_dropped`, `golden::when_in_single_line_function_body`, `rules::c1_when_inside_if_loop_and_function_bodies`, `rules::c1_positional_when_on_cancel_and_close_all` | `rules::c1_orders_without_when_are_untouched` |
| C2 | `golden::exit_pair_keeps_v5_precedence`, `golden::trailing_exit_pairs_keep_v5_absolute_precedence`, `golden::exit_pair_with_constant_absolute_drops_relative`, `golden::exit_pair_hoists_stateful_arguments_in_source_order`, `rules::c2_pairs_inside_an_if_block_and_a_function`, `rules::c2_explicit_na_absolute_is_dropped` | `rules::c2_unpaired_levels_are_untouched` |
| C3 | `golden::strategy_margins_default_to_zero`, `rules::c3_only_the_missing_margin_is_added` | `rules::c3_explicit_margins_and_indicators_are_untouched` |
| D1 | `golden::numeric_condition_is_cast_to_bool`, `golden::numeric_operand_of_boolean_equality_is_cast_to_bool`, `golden::numeric_branch_of_boolean_conditional_is_cast`, `rules::d1_numbers_used_as_conditions_in_functions_loops_and_switches` | `rules::d1_boolean_conditions_are_untouched` |
| D2 | `golden::bool_na_becomes_false`, `rules::d2_bool_na_in_locals_methods_switches_and_arrays` | `rules::d2_na_of_other_types_is_untouched` |
| D2g | `golden::nz_of_bool_is_removed`, `rules::d2g_fixnan_of_a_bool_inside_a_function` | `rules::d2g_nz_and_fixnan_of_numbers_are_untouched` |
| D2h | `golden::na_of_comparison_tests_operands`, `rules::d2h_comparison_test_inside_a_ternary_branch` | `rules::d2h_na_of_a_number_is_untouched` |
| D2i | `rules::d2i_history_of_a_never_na_bool_becomes_a_bar_count` | `rules::d2i_does_not_apply_to_a_comparison_that_can_be_na` |
| D3 | `golden::comparison_with_na_literal_is_false`, `rules::d3_na_comparisons_in_functions_and_ternary_branches`, `rules::d3_idiomatic_profile_writes_na_tests` | `rules::d3_na_calls_and_ordinary_comparisons_are_untouched` |
| D4a | `golden::na_for_special_constant_arguments`, `rules::d4a_na_scale_is_removed_from_the_declaration` | `rules::d4a_explicit_constants_are_untouched` |
| D4b | `golden::na_for_special_constant_arguments`, `rules::d4b_na_branch_in_a_function_ternary`, `rules::d4b_hline_style_na_branch_assumes_the_solid_style` | `rules::d4c_complete_conditionals_and_other_types_are_untouched` |
| D4c | `golden::missing_unique_style_branch_uses_v5_plot_default`, `golden::hline_style_switch_gains_a_default_arm`, `golden::function_returning_a_multiline_style_switch_keeps_its_default_arm`, `rules::d4c_function_ending_in_a_statement_switch_or_if`, `rules::d4c_statement_tail_findings_carry_their_class` | `rules::d4c_complete_conditionals_and_other_types_are_untouched` |
| D5 | `golden::const_int_division_is_truncated`, `golden::comments_and_layout_survive`, `rules::d5_const_divisions_in_functions_methods_branches_loops_and_requests`, `rules::d5_reuses_existing_parentheses_and_nests_truncations`, `rules::d5_leaves_parentheses_after_a_keyword_in_place`, `rules::d5_function_called_with_const_and_series_is_reported` | `rules::d5_divisions_that_are_not_const_int_are_untouched`, `rules::d5_library_export_with_a_typed_parameter_keeps_its_fraction` |
| D6 | `golden::reassigned_length_uses_first_bar_value`, `rules::d6_first_bar_value_inside_a_function` | `golden::non_persistent_global_length_keeps_weak_qualifier`, `rules::d6_lengths_that_are_never_reassigned_are_untouched` |
| D8 | `golden::linewidth_below_one_is_clamped`, `rules::d8_width_in_a_ternary_and_an_hline` | `rules::d8_widths_known_to_be_at_least_one_are_untouched` |
| D9 | `golden::final_assignments_in_function_value_branches_get_explicit_reads`, `golden::implicit_loop_step_is_made_explicit`, `rules::e4_bounds_in_functions_nested_loops_and_conditional_mutations` | `golden::side_effect_if_assignments_do_not_get_extra_reads` |
| E1 | `golden::stateful_and_operand_is_evaluated_first`, `golden::stateful_or_operand_inside_ternary_branch`, `golden::stateful_operand_in_else_if`, `golden::stateful_operand_in_while_condition`, `golden::stateful_operand_in_request_expression`, `golden::stateful_operand_in_switch_label`, `rules::e1_operands_in_functions_methods_and_loop_bodies`, `rules::e1_ternary_branch_inside_a_library_method` | `golden::pure_operands_are_left_alone`, `rules::e1_history_references_and_pure_calls_are_untouched` |
| E2 | `golden::implicit_loop_step_is_made_explicit`, `rules::e2_descending_loop_with_a_changing_start_bound` | `golden::negative_loop_step_retains_v5_runtime_error`, `golden::negative_input_loop_step_is_not_silently_made_positive`, `rules::e2_explicit_steps_and_for_in_loops_are_untouched` |
| E3 | `golden::compound_assignments_expand_without_changing_rhs_grouping`, `golden::compound_assignment_with_switch_rhs_keeps_the_block_expression_unwrapped`, `rules::e3_fields_methods_and_for_in_bodies` | `rules::e3_plain_reassignments_are_untouched` |
| E4 | `golden::loop_end_that_changes_is_fixed_at_entry`, `golden::reassigned_arithmetic_loop_bound_is_saved_once`, `golden::loop_result_expression_end_that_changes_is_fixed_at_entry`, `rules::e4_bounds_in_functions_nested_loops_and_conditional_mutations`, `behavior::loop_end_is_fixed_at_entry_when_the_body_changes_it` | `golden::loop_end_that_cannot_change_is_left_alone`, `golden::immutable_arithmetic_loop_bound_migrates_exactly`, `rules::e4_bounds_the_body_cannot_change_stay_inline`, `behavior::loop_end_the_body_cannot_change_stays_inline` |
| F1 | `golden::history_of_a_literal_keeps_leading_na`, `findings::idiomatic_profile_prefers_conventional_spellings`, `rules::f1_constants_in_functions_ternaries_and_colors` | `rules::f1_history_of_variables_is_untouched` |
| F2 | `golden::history_of_a_fresh_udt_field`, `rules::f2_persistent_object_is_snapshotted_and_fresh_object_is_indexed`, `rules::f2_fields_of_parameters_in_functions_and_methods` | `rules::f2_field_reads_without_history_are_untouched` |
| G1 | `golden::timeframe_period_comparison_and_display`, `rules::g1_comparisons_and_text_in_functions_switches_and_strings` | `rules::g1_numeric_timeframes_and_predicates_are_untouched` |
| G2 | `golden::recolored_constants_keep_v5_values`, `rules::g2_constants_in_functions_switches_inputs_and_tables` | `rules::g2_unchanged_constants_are_untouched`, `findings::idiomatic_profile_prefers_conventional_spellings` |
| G3 | `golden::label_text_color_keeps_v5_default`, `rules::g3_labels_inside_functions_and_blocks` | `golden::label_text_color_keeps_v5_default` |
| G4 | `golden::transp_folds_into_the_color`, `golden::conditional_plain_colors_preserve_legacy_transparency`, `golden::mixed_conditional_colors_preserve_each_branch_opacity`, `golden::plotarrow_transparency_preserves_both_explicit_colors`, `rules::g4_na_branch_keeps_its_transparency_on_the_color_branch`, `rules::g4_transp_on_fill_and_on_a_plotchar_without_a_color`, `rules::g4_runtime_color_alias_is_reported_with_its_span`, `findings::transp_without_a_documented_default_color_needs_review` | `rules::g4_calls_without_transp_are_untouched`, `golden::color_constructors_keep_their_own_opacity_even_at_zero_transparency` |
| H1 | `golden::globally_seeded_nested_request_pins_v5_context_and_reads_branch_value`, `rules::h1_explicit_false_is_removed_when_a_request_runs_locally` | `golden::ordinary_global_requests_keep_the_converter_default`, `golden::local_request_wrapper_keeps_dynamic_requests_enabled`, `rules::h1_request_in_an_if_expression_branch_keeps_dynamic_requests`, `rules::h1_scripts_without_requests_are_untouched`, `rules::h1_library_exporting_a_request_keeps_the_default` |
| I2 | `findings::modernize_rewrites_legacy_spellings`, `rules::modernize_findings_point_at_the_legacy_spellings` | `composition::option_matrix_composes_without_changing_division_policy` |
| I3 | `findings::modernize_rewrites_legacy_spellings`, `rules::modernize_findings_point_at_the_legacy_spellings` | `composition::option_matrix_composes_without_changing_division_policy` |
| I4 | `rules::modernize_findings_point_at_the_legacy_spellings` | `composition::option_matrix_composes_without_changing_division_policy` |
| D-1 | `findings::boolean_history_compared_with_false_needs_review`, `findings::na_of_a_boolean_is_approximated`, `findings::boolean_tuple_na_requires_manual_conversion`, `rules::review_findings_point_at_their_sites`, `rules::d2i_does_not_apply_to_a_comparison_that_can_be_na` | `findings::numeric_tuple_na_and_nonmissing_boolean_tuples_need_no_manual_conversion`, `rules::review_findings_do_not_fire_on_safe_sites` |
| D-2 | `findings::series_plot_offset_needs_review`, `rules::review_findings_point_at_their_sites` | `findings::constant_plot_offset_needs_nothing`, `rules::review_findings_do_not_fire_on_safe_sites` |
| D-3 | `findings::reassigned_length_that_cannot_be_folded_needs_review`, `rules::review_findings_point_at_their_sites` | `findings::input_derived_global_reassignment_does_not_need_length_review`, `rules::review_findings_do_not_fire_on_safe_sites` |
| D-4 | `findings::nested_request_that_needs_dynamic_requests_is_reported`, `rules::nested_request_finding_points_at_the_call_that_needs_dynamic_requests` | `rules::review_findings_do_not_fire_on_safe_sites` |
| D-5 | `findings::idiomatic_profile_prefers_conventional_spellings`, `rules::g1_idiomatic_text_use_is_reported_with_its_span` | `rules::review_findings_do_not_fire_on_safe_sites` |
| D-7 | `findings::negative_trailing_points_need_review`, `rules::review_findings_point_at_their_sites` | `rules::review_findings_do_not_fire_on_safe_sites` |
| D-8 | `findings::bars_back_on_another_timeframe_needs_review`, `rules::review_findings_point_at_their_sites` | `rules::b3_time_bars_back_in_ternary_branches_on_the_chart_timeframe`, `rules::review_findings_do_not_fire_on_safe_sites` |
| D-11 | `findings::index_from_a_search_is_reported_and_guarded_on_request`, `findings::negative_literal_index_is_reported_and_guarded_on_request`, `findings::reordered_named_array_index_is_reported_and_guarded`, `rules::review_findings_point_at_their_sites` | `rules::review_findings_do_not_fire_on_safe_sites` |
| D-12 | `rules::review_findings_point_at_their_sites` | `rules::review_findings_do_not_fire_on_safe_sites` |
| D-13 | `findings::na_strategy_direction_needs_review`, `rules::review_findings_point_at_their_sites` | `rules::review_findings_do_not_fire_on_safe_sites` |
