# Runtime

`pinewasm_vm` is a deterministic interpreter for validated CSIL
programs (see `docs/ir.md`). This document specifies the execution
model, the value and `na` semantics, resource limits, and error
reporting, and marks each semantic rule as **verified by test** (with
the test name) or **assumed** (implemented, but not pinned by a
dedicated test). Behavior claims are limited to what the repository's
tests pin.

## Bar-by-bar execution model

The unit of execution is one run of `main` per bar. The VM owns the bar
loop; the program never sees it:

```text
run(program, data, options):
    validate market data (equal-length OHLCV arrays,   # else MalformedMarketData
                          finite positive mintick)
    bars = min(data.bars, options.max_bars?)
    slot_ring = program.history_ring()  # power of two; covers max_bars_back,
                                        # literal slot offsets, and window lengths
    user_slots = [ [na; slot_ring] for each series slot ]
    written    = [ false      for each series slot ]   # "ever stored", persists across bars
    plots      = [ [na; bars] for each plot slot ]
    for bar in 0..bars:
        if bar > 0:
            for each persistent (`var`) slot s:        # carry-forward
                user_slots[s][bar & (slot_ring - 1)] =
                    user_slots[s][(bar - 1) & (slot_ring - 1)]
                # carry-forward does NOT set written[s]
        for each non-persistent slot s:
            user_slots[s][bar & (slot_ring - 1)] = na  # clear recycled cell
        stack.clear()
        pc = 0
        budget = options.max_instructions_per_bar
        loop:
            if budget == 0: fail InstructionLimitExceeded
            budget -= 1             # WINDOW_MEAN charges length-1 more (costs `length` total)
            execute code[pc]        # reads/writes may touch user_slots/plots at `bar`
                                    # STORE_USER_SLOT_CURRENT also sets written[slot]
                                    # JUMP_IF_SLOT_WRITTEN branches on written[slot]
            if RETURN: break
        # writes made this bar stay in the bounded slot ring and plot vectors
    truncate each plot series to the bars actually executed
    return { plots, errors, bars_executed }
```

The first runtime error aborts the run: the error is recorded, the loop
stops, and plots contain data for the bars completed before the failure
(`bars_executed` reports how many). Within a bar, statements execute in
program order; a value stored by `STORE_USER_SLOT_CURRENT` earlier in
the bar is visible to an offset-0 read later in the same bar.

An order lends the contiguous tail of the operand stack to the broker decoder
and then drops the consumed values. The same stack allocation and any earlier
operands survive success or a broker error. Short stacks retain the original
consume-then-underflow behavior. This avoids allocating, copying, and reversing
an argument vector for each order; resource charges and instruction addresses
are unchanged. The `order_operands_*` and `order_operand_underflow_*` tests
in `crates/pinewasm_vm/src/tests.rs` pin these ownership and error rules.

**User-defined functions leave no runtime trace.** A `name(p1, p2) => …`
definition is a compile-time template: the semantic analyzer **inlines** a
fresh copy of the body at every call site (per-call-site
instantiation), binding each argument to a hidden temp slot. There is **no
call frame, no call stack, and no function opcode** — by the time the VM runs,
a call has become ordinary inlined statements, and each instantiation's `var`
slots, `ta.*` state, and hidden history/result slots are distinct runtime
slots like any other. See [`language-support.md`](language-support.md#functions-user-defined).

## Storage: user series slots and plot slots

- **User series slots.** Every assigned variable owns a fixed-size history
  ring, `user_slots[slot]`, indexed by `bar & (ring - 1)`. The power-of-two
  ring covers the deepest observable slot read: the effective
  `max_bars_back`, every literal user-slot offset, and every
  `WINDOW_MEAN` length minus one. Assignment writes the _current_ bar's cell
  only. Before each bar, the VM resets a non-persistent slot's recycled cell to
  `na`, so a skipped assignment cannot expose the value left by an older bar —
  **verified by tests** `slot_history_is_a_ring_not_one_cell_per_bar` and
  `non_var_declaration_still_resets_per_bar` (`crates/pinewasm_vm/src/tests.rs`).
- **Compiled slot history.** Emitted modules allocate each variable's ring
  from that variable's literal history reads, window requirements, and
  persistent carry. A dynamic offset reserves its permitted history ceiling.
  An unused inlined temporary therefore does not receive a full 5,000-bar
  buffer. Native differential checks cover dynamic offsets, persistent object
  graphs, and objects read through history after a body is split into pieces.
- **Persistent (`var`) slots and carry-forward.** Each slot's `SlotDef`
  carries a `persistent` flag. At the start of every bar after the first,
  a persistent slot's current cell starts as the previous bar's _final_
  value (which may itself be `na` if the slot has never been written);
  non-persistent slots keep starting as `na`. Within the bar, stores
  overwrite the carried value as usual — **verified by tests**
  `var_persists_without_reassignment` and
  `var_counter_increments_across_bars`.
- **Written flags and initialize-once.** The VM keeps one `written: bool`
  per slot, persisted across bars. It is set by `STORE_USER_SLOT_CURRENT`
  and **never** by carry-forward. `JUMP_IF_SLOT_WRITTEN` branches on it,
  which gives `var` initializers run-once semantics: the initializer
  expression is evaluated only the first time the declaration statement
  actually executes. A `var` declaration inside an `if` branch therefore
  initializes on the first bar the branch runs — exactly once — and cells
  for bars before that first initialization read as `na` — **verified by
  test** `conditional_var_initializes_once_on_first_execution`. The
  conditional-first-execution rule is an **assumed** design choice,
  pinned by that test.
- **Hidden (synthetic) slots.** Every history-on-expression site
  (`(close - open)[1]`, `close[1][2]`, ...) owns one hidden, non-persistent
  series slot, named `%h0`, `%h1`, ... by the compiler. At runtime it is an
  ordinary user slot: when the site executes, the program stores the
  expression's current value into the slot's current-bar cell and reads the
  slot back at the offset (`STORE_USER_SLOT_CURRENT` + `READ_USER_SLOT` —
  no dedicated opcodes). Because the store only happens on bars where the
  site executes, a site inside an `if` branch leaves `na` **gaps** in its
  hidden slot for bars where the branch did not run, and offset reads
  across a gap yield `na`; the compiler warns with `W0004` at such sites.
  A site inside a `var` initializer is the limiting case: initialize-once
  means it executes exactly once, so its hidden slot records a single bar
  and offset reads over it are permanently `na` — `W0004` fires there too,
  with run-once wording (**verified by test**
  `compound_history_in_var_initializer_warns_w0004`,
  `crates/pinewasm_semantics/src/tests.rs`).
  The gap rule is a design choice (**assumed**) —
  **verified by tests** `compound_history_matches_variable_history`,
  `compound_history_is_na_on_bar_zero`,
  `history_of_history_composes_like_a_deeper_offset` (composition:
  `close[1][2]` equals `close[3]`), `nested_compound_history_hand_computed`,
  and `compound_history_in_a_branch_reads_gaps_as_na`
  (`crates/pinewasm_vm/src/tests.rs`).
- **Input-backed TA parameters are resolved before each run.** The VM
  resolves `RunOptions.inputs`, validates each integer against the builtin's
  range, and checks the selected history and TA sizes against its memory
  budget before cloning the program and allocating state. The caller's
  compiled program stays reusable. Wrong-kind overrides
  use the input default; valid integers outside the TA range fail with
  `InvalidTaLength`. Changing inputs during an existing realtime session
  requires starting a new session and replaying its history.

  Emitted WebAssembly and standalone WebAssembly modules read the same values from their input
  table before each `run`, reset all engine state, and meter the actual window
  sizes. A foreign host's `prepare` precedes input-table population, so the
  artifact reserves capacity for the builtin's legal maximum (usually 100000)
  for input-backed windows. This can use more memory than the VM's exact
  allocation; existing artifact memory limits still apply. Invalid lengths
  return status 12, mapped by standalone WebAssembly to `Indicator failure: TA input length out of
  range`. Existing compiled artifacts need recompilation once with this
  compiler to acquire the binding metadata.

  A `ta.ema` length computed from inputs (`n + 1`, `n * 2`, an alias of one)
  is not an input-backed parameter. It is a run-time length
  (`EMA_SIMPLE_DYNAMIC`): the call evaluates it when it executes, checks
  1..=100000, and keeps one seed-then-recurse smoother for its call site, with
  no source history. The VM and standalone WebAssembly modules run it natively
  and agree bit for bit; a standalone module reports a length outside the
  range with the same status 12, from the bar where the call ran, and starts
  the next request with fresh smoothers. A chart-tier WebAssembly module uses
  the VM fallback for it.
- **`ta.*` call-site state: one hidden slot per call site.** Every
  syntactic `ta.change(...)`/`ta.sma(...)` call site owns its own hidden,
  non-persistent slot (compiled exactly like the materialized-history slots
  above). When the site executes, the program stores the call's
  float-coerced source into the slot's current-bar cell and computes the
  result from the slot's history (`STORE_USER_SLOT_CURRENT` +
  `READ_USER_SLOT`s for `ta.change`; `STORE_USER_SLOT_CURRENT` +
  `WINDOW_MEAN` for fixed `ta.sma` lengths or `WINDOW_MEAN_DYNAMIC` for
  runtime integer lengths). Variable-length `ta.highest` and `ta.lowest`
  similarly use `WINDOW_EXTREMUM_DYNAMIC`. The dynamic opcodes evaluate length on each
  bar, check 1..=100000, and retain enough source history for any valid
  window. Native AOT currently uses the VM fallback for
  these opcodes. Two textually identical calls keep
  independent state — **verified by test**
  `ta_call_sites_keep_independent_state`. **Conditional-execution rule:**
  a call site inside an `if` branch stores nothing on bars where the branch
  does not execute, leaving `na` gaps in its state; a `ta.change` offset
  read or a `ta.sma` window that crosses a gap is `na`, and the compiler
  warns `W0004` at such sites (should be executed on every bar for
  consistent results). A call site inside a `var` initializer executes
  only once — one bar of state, result permanently `na` after the first
  bar — and warns `W0004` with run-once wording (**verified by test**
  `ta_call_in_var_initializer_warns_w0004`,
  `crates/pinewasm_semantics/src/tests.rs`). Uses the same gap rule as
  hidden history slots (**assumed**) — **verified by test**
  `ta_sma_in_a_branch_warns_and_gap_bars_produce_na_windows`. Warm-up:
  `ta.change` is `na` for the first `length` bars
  (`ta_change_default_length_hand_computed`,
  `ta_change_length_two_hand_computed`); `ta.sma` is `na` until `length`
  bars exist at the site and whenever any window value is `na` — the
  any-na-in-window rule is an **assumed** design choice, pinned by
  `ta_sma_warm_up_and_values_hand_computed` and
  `ta_sma_window_with_na_input_is_na`.
- **The ta engine: VM-owned state, one entry per call site.** Every
  **engine-backed** stateful call site (`fixnan`, `math.sum`, and the
  `ta.*` batch from `ta.ema` through the oscillators, cumulative volume
  studies (including the variable-style `ta.obv` family, referenced as
  bare names), and pivots — see `docs/language-support.md`)
  owns one entry of `ta_state: Vec<TaState>`, allocated from the program's
  `ta_states` declaration table at run start (state ids are assigned by
  the compiler in syntactic order; the validator guarantees each id is
  used by at most one `CALL_TA` site). Unlike the slot-based pair above,
  no hidden series slot is involved: the VM keeps **ring buffers** for
  window functions (generally rescanned on every advance rather than kept as
  running aggregates, so float cancellation error never accumulates;
  metered as the window length — see the limits table) and **scalar
  state** for recursive/O(1) functions (`ta.ema`/`ta.rma` seed-then-
  recurse accumulators, `ta.cum`'s running total, `ta.crossover`'s
  previous operands, ...). Ring storage is planned and charged against the
  run's memory limit before allocation. `ta.highest`, `ta.lowest`,
  `ta.highestbars`, and `ta.lowestbars` use a bounded monotonic candidate
  cache for lengths of at least 32. Its 24 indices fit inside the existing
  state allocation, preserving the previous memory admission limits. The
  cache retains the strongest prefix and rescans the window if that prefix
  expires after overflow. Smaller windows use an ordered scan.
  `f64::max`/`f64::min` leave the sign of a zero result to the target, so
  the engine fixes it, by two rules. **Window extrema never return
  `-0.0`:** a zero highest or lowest value is `+0.0`, whatever the signs of
  the zeros in the window (`ta.highest`, `ta.lowest`, and the folded
  extremes inside `ta.stoch`, `ta.wpr` and `ta.range`), for constant and
  for series lengths alike. This is not the
  IEEE 754 `minimum` of the window, which would be `-0.0` when a `-0.0` is
  present; it costs one comparison per bar instead of a second pass.
  **Once-per-bar maxima and minima follow IEEE 754-2019
  `maximumNumber`/`minimumNumber`** (`+0.0` is the larger zero, and a NaN
  operand yields the other operand): `ta.max`, `ta.min`,
  `ta.sar`, the `ta.vwap` bands, and `ta.tr` with the functions built on
  it. Either way the interpreter and compiled artifacts agree bit for bit.
  The existing
  window-length instruction charge remains a conservative per-call bound,
  including cache refills. Warm-up, missing-value poisoning, and newest-tie
  offsets are unchanged. SMA summation order is unchanged.
  Two textually identical calls keep independent state — **verified by
  test** `ta_engine_call_sites_keep_independent_state`
  (`crates/pinewasm_vm/src/tests.rs`).
  **Conditional-execution gaps freeze engine state.** A site's state
  advances **only when its `CALL_TA` instruction executes**. On bars
  where control flow skips the site, the state simply does not see that
  bar: windows and recursions resume over the bars the site actually
  executed on (this differs subtly from the slot-based story above, where
  skipped bars leave `na` cells that poison windows — the engine has no
  per-bar cells to leave gaps in; **assumed**). `W0004`
  warns at such sites exactly as for the slot-based functions, and a site
  inside a `var` initializer advances exactly once — **verified by tests**
  `ta_engine_state_freezes_when_its_branch_does_not_execute`
  (`crates/pinewasm_vm/src/tests.rs`) and
  `ta_engine_call_inside_a_branch_warns_w0004` /
  `ta_engine_call_in_var_initializer_warns_w0004`
  (`crates/pinewasm_semantics/src/tests.rs`).
  **Warm-up and seeding.** Window functions return `na` until `length`
  values have been received at the site, and an `na` input inside the
  window poisons it (`na` result) — except `ta.mom`/`ta.roc`, which need
  only the window's two endpoints. Recursive functions (`ta.ema`,
  `ta.rma`, and their derivatives `ta.rsi`, `ta.atr`) seed with the mean
  of the first `length` **non-na** inputs — equal to the sma on the seed
  bar (**verified by tests** `ta_ema_seeds_with_sma_then_recurses`,
  `ta_rma_seeds_with_sma_then_uses_wilders_alpha`) — then recurse; an
  `na` input yields an `na` output that bar but does **not** reset the
  state (**assumed**, pinned by
  `ta_ema_na_input_yields_na_without_resetting_state`). Per-function
  formulas and na rules are specified in `docs/language-support.md`.
- **Plot slots.** Every top-level `plot(...)` call site owns one output vector
  with one cell per bar, initialized to `na`. `PLOT_F64` writes the current
  bar's cell. A conditional series can select `na` for bars that should not
  display a value — **verified by test** `if_else_branches_choose_plot`
  (`crates/pinewasm_vm/src/tests.rs`). Plot-family calls in a local block are
  rejected with `E0254` before execution.
- Builtin series (`open`, `high`, `low`, `close`, `volume`) read
  directly from the input `MarketData` arrays; `bar_index` is computed.
- **`mintick`.** `MarketData` carries the instrument's minimum price
  increment, `mintick: f64` (serde default 0.01 for hosts that omit it;
  the serde types in [`crates/pinewasm_vm/src/lib.rs`](../crates/pinewasm_vm/src/lib.rs)
  are the authoritative field definitions). It must be finite and
  positive — anything else is `MalformedMarketData` before bar 0
  (**verified by tests** `invalid_mintick_is_a_structured_error`,
  `market_data_mintick_defaults_via_serde`). `math.round_to_mintick(x)`
  rounds to the nearest multiple: `round(x / mintick) * mintick`
  (**verified by test** `math_round_to_mintick_default_and_custom`).

## History lookup rule

For any series read with offset `n`:

```text
target_index = bar - n
if target_index < 0: value = na
else:               value = series[target_index]
```

- Builtin reads before bar 0 are `na`; user-slot reads before bar 0 or
  of never-written cells are `na`. **Verified by test**
  `user_series_history` (offsets 2 into a user slot produce
  `[na, na, 2.0, 4.0]`) and `core_example_produces_na_2_minus1`
  (`close[1]` on bar 0).
- History of a persistent (`var`) slot reads the per-bar cells like any
  other slot — carry-forward materializes the carried value into each
  bar's cell, so `n[1]` sees the previous bar's final value (`na` on
  bar 0) — **verified by test** `history_of_a_persistent_var`
  (`[var n = 0][n := n + 1][plot(n[1])]` plots `[na, 1, 2, 3]`).
- `bar_index[n]` evaluates to `target_index` as an **int** — the bar
  index _of the referenced bar_, not the current one. `bar_index` itself
  is 0-based and typed `series int` — **verified by
  test** `builtin_series_and_bar_index` and
  `bar_index_is_int_and_supports_modulo` (which pins the int typing via
  exact `bar_index % 2 == 0` arithmetic).
- Offsets may be a non-negative integer **literal** (fixed at compile
  time) **or** a series/simple `int`-typed **expression** evaluated at run
  time (`close[n]`, `n` an int variable/series). A non-`int` offset
  (string, float, `series float`) is still `E0210`. A dynamic offset is
  resolved against the slot's history each bar: a negative offset (future
  bar), an offset beyond the available history, or an `na` offset reads
  `na` (never a panic) — see `READ_BUILTIN_DYNAMIC`/`READ_USER_SLOT_DYNAMIC`
  in docs/ir.md. A non-negative dynamic offset greater than the effective
  `max_bars_back` ceiling raises `HistoryBufferLimitExceeded`; a dynamic offset
  within the ceiling but before bar 0 still reads `na`.
  **Verified by** `dynamic_history_offset_reads_the_correct_historical_bar`,
  `dynamic_history_offset_beyond_history_is_na`,
  `dynamic_history_offset_negative_is_na`, and
  `dynamic_history_offset_na_offset_is_na`.

## Time axis and market metadata

`MarketData` carries, besides OHLCV, a **time axis** and **symbol
metadata**:

- `time: Vec<i64>` — each bar's **open** time in epoch milliseconds. When
  omitted or empty it is **synthesized** as a deterministic daily sequence
  starting `2020-01-01T00:00:00Z` (epoch `1_577_836_800_000` ms) with one
  `86_400_000` ms step per bar. A supplied axis is validated: it must have
  one entry per bar and be **strictly increasing** (`MalformedMarketData`
  otherwise). On the first appended realtime bar, a synthetic historical axis
  is materialized before the host tick's `time` is appended; the tick must be
  later than the synthesized final history open, and its derived close time
  must fit in `i64`. — **verified**:
  `synthetic_time_axis_is_a_daily_sequence_from_2020`,
  `supplied_time_axis_is_used_verbatim`, `non_monotonic_time_axis_is_malformed`,
  `wrong_length_time_axis_is_malformed`,
  `overflowing_market_timestamps_are_structured_failures`.
- `timeframe: String` — a timeframe string (default `"1D"`). Its length
  in seconds (`"1S"`=1, bare `"5"`=300 minutes-form, `"1D"`=86 400,
  `"1W"`=604 800, `"1M"`=nominal 30 days) drives `time_close`; an
  unrecognized string falls back to one day. — **verified**:
  `timeframe_in_seconds_matrix`, `time_close_tracks_the_timeframe_string`.
- `timeClose: Vec<i64>` — optional authoritative chart closing instants.
  These override the nominal built-in `time_close` series, must follow their own opens, and must
  not overlap the next supplied bar. `requestTimeClose: HashMap<String, Vec<i64>>`
  optionally supplies the requested bar's closing instant on each chart bar,
  keyed by a timeframe string such as `"1W"`. Equal closing instants identify
  one requested bucket; prices still aggregate from the chart's OHLCV.
  These lanes require complete bar alignment and chart `timeClose`; malformed
  selectors, duplicate timeframe identities, decreasing closes, and a new
  bucket beginning before the previous close are rejected before execution.
  Owned clock storage and lookup work count toward VM resource limits.
  Realtime updates to an existing bar retain its clock; appending a new bar
  with these lanes fails before feed mutation and requires rebuilding a
  complete snapshot. — **verified**:
  `security_authoritative_clocks_defer_missing_days_and_hide_incomplete_bars`,
  `authoritative_clocks_reject_incomplete_or_inconsistent_host_lanes`,
  `authoritative_clock_storage_is_budgeted_and_realtime_append_fails_closed`.
`requestBars: HashMap<String, RequestTimeframeBars>` optionally supplies a
complete same-symbol OHLCV history for a requested timeframe. Each feed uses
equal-length `time`, `timeClose`, `open`, `high`, `low`, `close`, and `volume`
arrays, ordered oldest first and spanning the full chart history. When present,
it supplies actual requested-bar values and history, including periods omitted
from a sparse chart feed; an equivalent `requestTimeClose` lane must agree with
its close times. These feeds are validated and included in VM memory accounting.
Realtime append requires rebuilding the complete snapshot. — **verified**:
`security_uses_complete_host_htf_bars_when_chart_history_is_sparse`.
- `syminfo` — the symbol-metadata table; the defaults given here are PineWASM's own host defaults. Strings `ticker`, `currency`,
  `type` (serde `kind`), `timezone` (default `"UTC"`), plus the
  additions `basecurrency`, `description`, `country`, `industry`, `sector`,
  `root`, `prefix`, `session`, `volumetype`, `main_tickerid` (each default
  `""`); and numerics `minmove` (default `1`), `pricescale` (default `100`),
  `pointvalue`/`mincontract` (default `1.0`). `mintick` lives on `MarketData`
  itself (it predates this struct). Each backs the same-named `syminfo.*`
  accessor as a per-run constant. The fundamentals / recommendations / target
  surface (`employees`, `shareholders`, `shares_outstanding_float`/`_total`,
  the `recommendations_*` and `target_price_*` families) are optional
  `Option<i64>`/`Option<f64>` fields — `na` by default, host value when
  supplied (see the fundamentals note below).

The time-axis **built-in variables** are all reads of the shared
`ReadBuiltin` opcode — no new CSIL was needed (the `BuiltinSeries` enum grew;
the int/float distinction is a single `BuiltinSeries::is_int()` source of
truth consulted by both the resolver and the validator):

| Variable                                                         | Type           | Value                                                       |
| ---------------------------------------------------------------- | -------------- | ----------------------------------------------------------- |
| `time`                                                           | `series int`   | bar open time (epoch ms)                                    |
| `time_close`                                                     | `series int`   | `time + timeframe_in_seconds × 1000`                        |
| `timenow`                                                        | `series int`   | **deterministic**: the last bar's _close_ time on every bar |
| `last_bar_index`                                                 | `series int`   | `bar_index` of the run's last bar (respects `max_bars`)     |
| `last_bar_time`                                                  | `series int`   | _open_ `time` of the run's last bar                         |
| `hl2` / `hlc3` / `ohlc4` / `hlcc4`                               | `series float` | derived OHLC averages (`hlcc4` double-weights close)        |
| `year`/`month`/`dayofmonth`/`dayofweek`/`hour`/`minute`/`second` | `series int`   | calendar fields of `time` in the exchange timezone          |

**`timenow` is a fixed value.** It is not the wall-clock time of the latest
chart update: a wall clock would make runs non-reproducible, so this VM pins
it to the last bar's close time — the same value on every bar. Reproducibility is a hard project constraint (cf.
`math.random`). — **verified**: `time_builtins_over_a_crafted_5_bar_feed`.

**Calendar math** uses Howard Hinnant's `civil_from_days` (proleptic
Gregorian, valid for all years) after shifting the epoch-ms value into the
exchange timezone by the offset in force **at that instant**. `dayofweek`
numbers days 1 = Sunday … 7 = Saturday, matching the
`dayofweek.*` constants — **verified**: `dayofweek_constants_have_v6_numbering`.

### Exchange time zones and sessions

A timezone, whether `syminfo.timezone` or a script's argument, is either a
fixed offset in the forms PineWASM reads (`"UTC"`, `"GMT"`,
`"UTC-5"`, `"GMT+0530"`, `"+05:30"`; hours to 14, minutes to 59) or an IANA
name (`"America/New_York"`, matched without regard to ASCII case). An IANA
zone follows every historical offset change and its current
daylight-saving rule, so `hour`, `dayofweek`, `str.format_time`,
`timestamp(timezone, ...)`, `time(timeframe, session, timezone)` and every
higher-timeframe bucket read the clock the exchange actually kept.

- **One implementation.** The `pinewasm_tz` crate (`no_std`, no
  allocation, no panics) reads TZif data (RFC 8536) and evaluates the POSIX
  rule in its footer. The interpreter, the compile-time folding of
  `timestamp`, and the emitted standalone runtime all call it.
- **One pinned release.** The rules are IANA Time Zone Database release
  **2026e** (`pinewasm_tz::TZDATA_VERSION`), embedded through the exactly
  pinned `jiff-tzdb =0.1.9` crate (Unlicense OR MIT; compiled by its
  publisher with `zic`, rearguard data, slim format), and reported by
  `version()` as `tzdataVersion`. A new release is a reviewed change of that
  pin, which a test keeps equal to the constant, and a compiler release; a
  lockfile refresh cannot change what a compiler version computes.
- **Cross-checked with a second reader.** `jiff` evaluates the same
  TZif bytes for every one of the 599 names at each transition and a second
  either side, and on a monthly grid from 1850 to 2100, with no difference
  (`offsets_match_jiff_on_the_bundled_tzif`). A one-time
  cross-check during development, against Python's `zoneinfo` reading a
  separately compiled system copy of 2026e, agreed on all 1,753,152 offsets
  sampled every six hours from 1990 to 2040 in 24 exchange zones.
- **Wall-clock readings.** `timestamp(timezone, ...)` reads its fields as
  the zone's wall clock. A reading that occurs twice (clocks going back) is
  the earlier instant; one that never occurs (clocks going forward) is read
  with the offset before the change, so 02:30 on a night New York skips to
  03:00 becomes 03:30 — the rules of `java.time.ZonedDateTime.of`, checked
  against `jiff`'s compatible disambiguation.
- **No guessing.** A name that is neither form is refused: market data
  naming one fails `MarketData::validate` (`MalformedMarketData`), and a
  script naming one stops the run (`Unsupported`).
`syminfo.session_hours` declares the hours a symbol trades as a
session specification on the exchange's clock (`"24x7"`, `"0930-1600"`,
`"1700-1600:1234567"`). Where a higher-timeframe bucket starts depends on the
session: intraday buckets can start at the session's
open, and a session that opens the evening before belongs to the next day. Until
the bucket start is settled for each case, `time(timeframe)`,
`timeframe.change`, `request.security` and `ta.pivot_point_levels` group bars
only where the session leaves no doubt:

| Buckets | Grouped when | Otherwise |
| --- | --- | --- |
| No longer than the chart's timeframe | always: each bar is its own bucket | — |
| Under a day (`60`, `240`) | the session is `24x7`, or none is declared and the exchange keeps UTC, and the exchange's clock does not change during the bars | refused |
| One day | as above, or the session opens and closes within one local day, or the host supplies a `tradingDay` lane (which then groups the days) | refused |
| Several days (`2D`) | the session is `24x7`, or none is declared and the exchange keeps UTC | refused |
| Weeks, months | as for one day | refused |

A zone that has only ever kept one offset (`"Etc/UTC"`, a lower-case
`"utc"`) is that fixed offset. A host that supplies the requested
timeframe's own bars (`requestBars`, `requestTimeClose`) settles that
timeframe for `request.security` whatever the session;
`ta.pivot_point_levels` groups the chart's own bars, and with a
`tradingDay` lane its daily anchor follows the lane. A refusal
stops the run when the grouping first executes (`Unsupported`, naming the
timeframe and the session). Grouped buckets walk from local midnight on the
exchange's clock, daylight-saving changes included.

**Size and host requirements.** The embedded database (TZif data, a name table
and the reader) rides in the interpreter and so in the JavaScript package:
measured on the release build, the embedded database adds 464,761 bytes
(+4.8%) to `pinewasm_bg.wasm`, reaching 10,120,293 bytes, or 145,076 bytes
(+4.4%) gzipped, reaching 3,462,227 bytes. A standalone module carries none of it (`docs/aot.md`): its runtime
links only the fixed-offset and session readers. An IANA exchange or argument
follows its own clock rather than UTC, `timestamp` with an IANA zone folds to the right
instant at compile time, and a feed on a non-UTC exchange must declare its
session for higher-timeframe buckets. Hosts must send a timezone PineWASM
knows and, for those buckets, `session_hours`.

The top-level **function** forms are all implemented. `timestamp(...)` and
the calendar functions `year(t)`/`hour(t)`/… decompose an arbitrary epoch-ms
`time` (verified). The re-bucketing forms
`time(timeframe, session, timezone, bars_back, timeframe_bars_back)` and
`time_close(timeframe, session, timezone, bars_back, timeframe_bars_back)` return
the enclosing timeframe bucket's OPEN (`time`) or CLOSE (`time_close`) epoch-ms
time — modeled via the same RESAMPLE machinery as `request.security`, plus
runtime session filtering. The chart feed is resampled to `timeframe`
(memoized once per run per timeframe, then extended by each realtime-appended
bar; see the realtime notes below), `bars_back` first shifts the chart bar,
and `timeframe_bars_back` shifts the selected higher-timeframe bucket. The
`timeframe`, `session`, and `timezone` arguments are runtime strings; invalid
runtime values return `na`, not a panic. Session strings support `24x7`,
comma-separated `HHMM-HHMM[:days]` windows, overnight spans, and fixed-offset
or IANA timezones (read at the bucket's open; the close is converted back by
the zone's own rules); `time()` returns `na` outside the active session, while
`time_close()` clips the close to the active session boundary when a bucket
extends past it. **Verified:** `time_function_daily_returns_each_bar_enclosing_bucket_open`,
`time_close_function_is_bucket_open_plus_one_day`,
`time_function_weekly_buckets_share_one_open_time`,
`time_function_dynamic_timeframe_runs_at_runtime`,
`time_functions_apply_chart_timeframe_offsets`,
`time_functions_filter_regular_session_and_clip_close`,
`time_functions_interpret_session_timezone`,
`time_functions_return_na_for_invalid_dynamic_session`.

### Context accessors: `timeframe.*` / `syminfo.*` / `session.*`

These are more `ReadBuiltin` reads — no new opcode. `BuiltinSeries` gained a
third type partition, `is_string()`, alongside `is_int()`/`is_bool()`, so the
resolver and the CSIL validator agree on the string-typed accessors from one
source of truth. All are **per-run constants** (the same value on every bar)
except the `session.is*bar*` predicates, which track the run's first/last bar.

- **`timeframe.*`** derive from `MarketData::timeframe` (default `"1D"`):
  `period`/`main_period` (string, the tf itself), `multiplier` (int, the
  numeric part), and the unit predicates `isdaily`/`isweekly`/`ismonthly`/
  `isdwm`/`isintraday`/`isminutes`/`isseconds` (bool). `isticks` is always
  false (no tick timeframes). The functions `timeframe.in_seconds(tf)` (reuses
  `timeframe_in_seconds`) and `timeframe.from_seconds(seconds)` (canonical
  form: whole days `"<n>D"`, whole minutes `"<n>"`, else `"<n>S"`) are `CALL_PURE`
  builtins. `timeframe.change(tf)` returns `true` on the first chart bar of
  each `tf` bucket. The analyzer lowers every call site to ordinary
  statements over `time(tf)`: a hidden `var int` holds the opening time the
  call saw when it last executed, and the call is `true` on its first
  execution and whenever `time(tf)` differs from it. Buckets are therefore
  `time`'s (`resample::TimeframeBuckets`, shared with `request.security`):
  calendar months and multiples of them start on the first trading day of
  the month (the trading-day lane, else the bar's close date in exchange
  time), weeks start on Monday and multi-week blocks restart at the year's
  first Monday, a fixed width under a day walks from local midnight, and a
  width of a day or more walks from the first bar's local midnight. A
  timeframe shorter than the chart's makes `time` `na` and every bar a
  change; the chart's own makes every bar its own bucket. A literal or
  `const` `tf` is passed to `time` as a literal; any other is evaluated on
  every execution. Bucketing by `floor(time_ms / (tf_seconds × 1000))` is not used
  for this call.
  With no argument, `timeframe.in_seconds()` measures the chart timeframe.
- **`timestamp(...)`** is a `CALL_PURE` builtin with two overloads,
  selected by the first argument's type, or by the argument names in a named
  call: `timestamp(year, month, day, hour = 0, minute = 0, second = 0)` (UTC)
  and `timestamp(timezone, year, month, day, hour = 0, minute = 0, second = 0)`
  (leading fixed-offset zone). It computes UNIX
  **ms** via `epoch_ms_from_civil` — Howard Hinnant's `days_from_civil`, the
  exact inverse of the `civil_from_epoch_ms` calendar decomposition used for
  `year`/`month`/`day`/… — so `timestamp(2021, 1, 1) == 1_609_459_200_000`.
  The fields are a wall-clock reading in the zone, a fixed offset or an
  IANA name (see "Exchange time zones and sessions"); an unknown name stops
  the run. An `na`
  field yields `na`, and so does i64 overflow at any step of the calendar
  arithmetic (month carry, day count, seconds/milliseconds products), in debug
  and release builds alike; out-of-range month/day roll over (assumed). The date-string overload is a deferred matrix gap.
- **`ticker.*`** — the nine interface functions (`new`/`modify`/
  `heikinashi`/`kagi`/`linebreak`/`pointfigure`/`renko`/`standard`/`inherit`)
  are `CALL_PURE` builtins returning a **deterministic canonical ticker-id
  string** built from their arguments (`<prefix>:<arg0>[:<arg1>…]`;
  `eval_ticker`). na-propagating. **Matrix note:** real symbology resolution is
  a host concern — the id is an opaque canonical token, not a live symbol
  string; the signatures are PineWASM's own canonical form.
- **`syminfo.*`** are host-or-default reads off `MarketData::syminfo` /
  `mintick` (see the input struct above). A host that omits a field gets the
  listed default. `syminfo.expiration_date` (epoch ms, `series int`)
  reads the optional `SymInfo::expiration_date` — `na` by default for a
  non-expiring symbol. — **verified**:
  `syminfo_expiration_date_default_na_and_host_supplied`.
- **`syminfo.*` fundamentals** are company fundamental / analyst
  data the headless VM reads from optional `SymInfo` fields — **`na` by default**
  (there is no live fundamentals feed), host value when supplied. Names are listed in the
  coverage matrix. Integer counts / epoch-ms
  dates are `series int` (`employees`, `shareholders`, the `recommendations_*`
  counts + `recommendations_date`, `target_price_estimates`, `target_price_date`);
  share counts and analyst price targets are `series float`
  (`shares_outstanding_float`/`_total`, `target_price_average`/`_high`/`_low`/`_median`).
  — **verified**: `syminfo_fundamentals_default_na_and_host_supplied`.
- **`bid` / `ask`** (MISC) are `series float` reading the optional per-bar host
  series `MarketData::bid` / `ask`; `na` by default (this VM has no tick feed). A short host series reads
  `na` past its end. — **verified**: `bid_and_ask_default_na_and_read_host_series`.
- **`chart.left_visible_bar_time` / `chart.right_visible_bar_time`** (MISC) are
  `series int` (epoch ms) reading the optional host viewport values
  `MarketData::chart_left_visible_bar_time` / `chart_right_visible_bar_time`;
  `na` by default (the headless VM processes the whole feed and has no viewport).
  — **verified**: `chart_visible_bar_time_default_na_and_host_supplied`.
- **`input.price` / `input.time`** (MISC) are host-override-or-default inputs
  (`InputKind::Price`, a `float`; `InputKind::Time`, an `int` epoch ms): chart
  click-to-place is a host concern, so each returns its `defval` or a host
  override through the ordinary `RunOptions::inputs` map. — **verified**:
  `input_price_returns_defval_and_host_override`,
  `input_time_returns_defval_and_host_override`.
- **`session.*`** model a single continuous **regular** session in this
  headless VM: `isfirstbar`/`isfirstbar_regular` are true on bar 0,
  `islastbar`/`islastbar_regular` on the final bar, `ismarket` is always true,
  and `ispremarket`/`ispostmarket` always false (**assumed** — no intraday
  session boundaries or extended sessions). `session.regular`/`session.extended`
  are const-string tokens (modeled like `format.*`). `time_tradingday` is the
  epoch-ms start of the bar's trading day (local midnight in the exchange
  timezone). — **verified**: `timeframe_period_and_isdaily_default_to_one_day`,
  `timeframe_predicates_track_the_chart_timeframe`,
  `timeframe_in_seconds_and_from_seconds_round_trip`,
  `syminfo_string_reads_default_and_host_supplied`,
  `syminfo_numeric_reads_defaults`, `session_first_and_last_bar_predicates`.

## The value model

Runtime values are `pinewasm_mir::Value`:

```text
Value ::= Na | Bool(bool) | Int(i64) | Float(f64) | String(string)
        | Color{r,g,b,t} | Ref(HandleId)
```

`na` is a first-class value, not a raw NaN convention. Non-finite floats
are normalized to `Na` at every boundary through `Value::from_f64`:
market-data reads, `PUSH_F64`, arithmetic results, and JSON
deserialization. NaN and infinity therefore never exist inside the value
domain.

**Color model (transparency, not alpha).** `Color{r,g,b,t}` stores the
three `0..=255` RGB channels plus **transparency** `t` in `0..=100`
directly as a floating-point percentage. `t = 0` is fully opaque, `t =
100` fully transparent. Paint serialization normalizes `1 − t/100` to
four decimals and then rounds `255 * alpha` to the nearest byte, according to
the test-pinned paint serialization contract. A `#RRGGBBAA`
literal becomes `t = 100 * (1 − alphaByte/255)` at parse time; all 256
literal alpha bytes round-trip exactly. Constructors, input defaults and
overrides, gradient transparency and `ta.valuewhen()` color captures retain
fractions until the paint boundary. Color accessor values use
PineWASM's rounding rule. A
`color.new(c, transp)` value and a named constant with the same channels are
bit-identical and plot the same — **verified by test**
`color_new_value_plots_identically_to_a_named_color`
(`crates/pinewasm_vm/src/tests.rs`). The `color.*` functions
(`color.new`/`rgb`/`from_gradient`, `color.r/g/b/t`) run on the pure-builtin
engine and **clamp** every out-of-range channel/transparency argument rather
than wrapping or panicking — **verified by tests**
`color_rgb_clamps_out_of_range_channels_without_panicking` and
`color_from_gradient_clamps_out_of_range_value`. `color.from_gradient`
interpolates all four channels linearly and pins to the endpoint colors
outside `[bottom_value, top_value]`.

Current source emits float-returning `color.r/g/b/t` operations.
Integer-returning bytecode tags and whole-percentage JSON numbers remain
readable. Native ABI v6 still carries a whole transparency percentage in its
packed RGBT byte: fractional or unproved producers use the VM fallback, and fractional input overrides must reroute before native execution.
The alpha rule has focused executable tests.

`Int` carries exact 64-bit semantics: integer arithmetic is checked, and
overflow (as well as division/remainder by zero) yields `Na` rather than
wrapping — **verified by tests** `int_overflow_is_na`,
`unary_minus_overflow_is_na`, and `division_and_modulo_by_zero_are_na`
(`crates/pinewasm_vm/src/tests.rs`); the overflow-to-`na` rule is an
**assumed** design choice, pinned by those tests.

- Non-finite _input data_ reads as `na` — **verified by test**
  `non_finite_market_data_reads_as_na`.
- The `from_f64` normalization itself — **verified by test**
  `non_finite_floats_become_na` (`crates/pinewasm_mir/src/value.rs`).

The Rust JSON format maps `Na` to `null`, `Bool` to boolean, `Int` and `Float`
to JSON numbers, and `String` to string — **verified by test**
`json_round_trip` (`crates/pinewasm_mir/src/value.rs`) and
`golden_close_difference_example` (`crates/pinewasm_compiler/tests/pipeline.rs`,
asserting the plot serializes as `[null,2.0,-1.0]`). That JSON mapping is
deliberately asymmetric: `Int` serializes as a number, but whole JSON numbers
deserialize to `Float` — **verified by test**
`int_serializes_as_number_but_deserializes_as_float`
(`crates/pinewasm_mir/src/value.rs`). The direct WebAssembly binding does not
round an `Int` through JSON: it emits Rust `i64`/`u64` fields as JavaScript
`bigint`. See [browser.md](browser.md#wire-rules) for the JavaScript wire and
the required JSON-persistence codec boundary.

## Collections: the reference-value heap

The **`array<T>` core**, **`map<K, V>` operations**, and **`matrix<T>`
operations** are all implemented end-to-end: the VM instantiates
a `Heap` per run from `RunOptions.max_collection_elements` and executes the
`array.*`, `map.*`, and `matrix.*` operations through the single generic
`CALL_COLLECTION` opcode (see [ir.md](ir.md)), reachable via namespace calls,
method calls (`a.push(x)`), `<type>` generic constructors, and `for … in`
iteration. The reference-value model below applies to all three shapes.

Collections (`array`, `matrix`, `map`) are **reference-typed**. A collection value is a `Value::Ref(HandleId)` — a handle into a
per-run heap — **not** the collection's contents. The heap lives in
`pinewasm_mir::heap`:

```text
Heap       = arena of HeapObj, indexed by dense HandleId (from 0)
HeapObj   ::= Array  { elem_ty, items }
            | Matrix { elem_ty, rows, cols, items }   // row-major
            | Map    { key_ty, val_ty, entries }       // insertion-ordered
            | Object { type_id, fields }               // user-defined record
ElemType  ::= float | int | bool | string | color
            | object(type_id)                          // an object type, by its type_id
```

**Reference semantics.** Assignment copies the handle, so two variables can
alias one heap object and observe each other's mutations — this is
the intended behavior, not a value copy. The value-level half
(handle copy = aliasing) is **verified** by
`ref_handle_copy_aliases_same_object` (`crates/pinewasm_mir/src/value.rs`); the
heap-level half (a push through one handle is visible through an aliased
handle) is **verified** by `aliasing_two_handles_observe_one_mutation`
(`crates/pinewasm_mir/src/heap.rs`).

**Lifetime.** The VM collects unreachable heap objects at bar boundaries when
the arena or element budget reaches a threshold. It marks persistent slots,
the cells each slot can still read, operand values, and live
drawings. An unrelated dynamic numeric window can enlarge the shared history
ring without keeping every short-lived collection in it alive. A `var`-held
collection persists through its carried-forward handle. These cases are
verified by `unrelated_dynamic_window_does_not_pin_bar_local_arrays`,
`collection_history_reader_survives_unrelated_dynamic_window_gc`, and
`persistent_collections_survive_collection`; handle aliasing is verified by
`aliasing_two_handles_share_mutations`.

**Element limit.** `Heap::reserve` charges each grow against
`max_collection_elements` (default 100,000; see the resource-limits table)
and fails with a structured `HeapLimitError` — never a panic, never
unbounded growth — before mutating; shrinks call `Heap::release` to return
budget. This is the heap-growth-bomb guard (e.g. a loop pushing past the cap).
The VM maps `HeapLimitError` to a structured `CollectionLimitExceeded`
`RuntimeError` — **verified** end-to-end (a `for` loop pushing past a small cap
aborts without hanging) by `heap_limit_bomb_is_structured_error_no_hang`
(`crates/pinewasm_vm/src/tests.rs`); the limit mechanism itself is **verified** by
`reserve_enforces_limit_with_structured_error`,
`alloc_that_overflows_limit_fails_without_allocating`, and
`zero_limit_rejects_every_growth`.

**Structured error kinds.** Supported user-program failures use structured
errors; a panic is a defect, not part of the public contract. Collection
operations raise three `RuntimeErrorKind`s: `CollectionIndexOutOfBounds`
(an out-of-range `array.get`/`set`/`insert`/`remove`/`slice`/`fill` index, or
`first`/`last`/`pop`/`shift` on an empty array; `get`/`set`/`remove` count a
negative index from the end, so only an index outside `[-size, size)` fails), `CollectionLimitExceeded` (above), and `NaMethodReceiver` (calling a
method on an `na` array **or** `na` map — an unassigned collection variable is
`na`; `map.*` has no index errors, but a lookup on an `na` map is this error).
Each is a structured error that aborts the run at that bar (`bars_executed`
reflects the abort); all three are **verified** (`oob_get_is_structured_error_and_aborts`,
`oob_set_insert_remove_are_structured_errors`, `pop_on_empty_is_structured_error`,
`na_method_receiver_is_structured_error`). `na(arr)` on a live array is `false`
(`na_of_array_works`).
A missing (`na`) index is distinct: `array.get` returns `na`, and `array.set`
leaves the array unchanged, covered by local regression tests and
`missing_array_index_returns_na_and_makes_set_inert`.

**Wire format.** A raw handle is meaningless outside its run and never appears
as a plot value (plots are scalar), so `Ref` serializes to `null` — the same
one-way policy as `Int`/`Color` — **verified** by
`ref_serializes_as_null_and_renders_as_handle`.

**Displaced table identity.** A table displaced by a new table at the same
position remains `na`-observable, but collection search retains its original
lifetime identity. Thus two distinct displaced tables remain distinct in
`array.indexof`, `array.lastindexof` and `array.includes`; a literal `na`
does not match either one. The
[test source](../crates/pinewasm_compiler/tests/retired_table_identity.rs)
pins this distinction, with additional checks for aliases, maps,
matrices, object fields and copies. Scalar comparisons still observe the
displaced value as `na`, and its wire representation remains `null`.

The identity is assigned to the owning table object, preserved by in-process
heap cloning, and copied into a retired value without retaining a heap root.
Its checked run-local counter does not rewind on rollback or reset at AOT
host-state boundaries. Serialized `Heap`/`HeapObj` data omits this metadata;
it is structural data, not a resumable identity-preserving VM snapshot.

**Realtime rollback.** The realtime segment (see below) snapshots mutable
per-bar state before each re-executed tick. The VM clones the bar-start heap.
On rollback it preserves the transitive object graph reachable from all
`varip` reference-valued slot cells, overlays mutations to preexisting objects
at their stable handles, and packs reachable post-snapshot objects into a dense
suffix while rewriting their nested references. Other non-`varip` heap
mutations are rolled back, and unreachable tick-local allocations are dropped.
The merge checks the configured element and live-object caps and reports a
structured runtime error if the preserved graph cannot fit. Cost includes the
heap snapshot clone plus traversal and copying of the reachable `varip` graph
on each rollback; snapshots and overlay copies are outside the deterministic
`max_vm_bytes_per_run` counter.

**Nested collections** are not representable at the value layer (`ElemType`
has no collection case: it admits the five scalar types and an object type).
The language forbids nested collections except through UDTs; the
type checker rejects them with `E0234` (tracked in the coverage matrix).

**User-defined-type objects** are the fourth `HeapObj` shape,
`Object { type_id, fields, table_identity }`, and are **reference values on the same heap** — so
they inherit every property above for free. A `type Name` declaration records one
`ObjectTypeDecl { name, field_count }` in the program's `object_types` table;
`Name.new(...)` allocates a fresh `Object` whose `fields` vector holds the field
values in declaration (slot) order and pushes a `Value::Ref`. Because objects are
handles: assignment copies the handle (two variables alias one object — a
mutation through one is visible through the other, **verified** by
`udt_aliasing_two_vars_one_object`), `var`-held objects persist across bars
(**verified** by `udt_field_write_across_bars_via_var`), and the realtime-bar heap
snapshot rolls a mutated object back on a discarded tick exactly as it does a
collection. `Name.copy(obj)` is a **shallow** copy: a NEW handle whose `fields`
are cloned, independent of the source (**verified** by `udt_copy_is_independent`).
Field values may themselves be `Value::Ref` (a UDT- or collection-typed field), so
objects form graphs — a self-referential `type Node { int val; Node next }`
builds a linked list walked through the `next` chain (**verified** by
`udt_self_referential_linked_list_walk`). An **unassigned object variable is
`na`**; reading or writing its field is the structured `NaFieldAccess` runtime
error (never a panic — **verified** by `udt_na_object_field_read_is_structured_error`
and `udt_na_object_field_write_is_structured_error`). Scalar field typing is
enforced statically by the checker (`E0236`/`E0237`); the heap stores field values
positionally, so field access is an O(1) indexed read/write on the `fields` vector.

## NA semantics

`and`/`or` **short-circuit**: the rhs is skipped exactly when
the lhs alone decides (`false and …`, `true or …`), so a guarded rhs never
runs — `array.size(a) == 0 or array.get(a, array.size(a) - 1) != x` is safe
on an empty array (**verified** by `or_short_circuits_a_true_lhs_before_a_faulting_rhs`,
`and_short_circuits_a_false_lhs_before_a_faulting_rhs`), and a stateful
`ta.*` call in the rhs does not execute on skipped bars (the behavior
W0008 warns about — **verified** by `lazy_or_skips_a_stateful_ta_call_in_the_rhs`).
This holds even when the rhs desugars into statements — an `if`/`switch`
expression chain or an inlined user-function's leading statements/argument
temps — which the semantics layer would otherwise hoist ahead of the operator:
such an rhs lowers to a real branch (`if L == true/false …`) so those
statements run only when the lhs does not short-circuit (**verified** by
`or_short_circuits_hoisted_rhs_statements_from_an_inlined_udf` and its `and`
mirror). An `na` lhs still evaluates the rhs so the strong-Kleene combine below
can decide (**verified** by `na_lhs_still_evaluates_rhs_for_kleene_logic`; this
records the compiler's existing behavior.
The ternary `?:` is lazy: a ternary with a non-literal arm lowers
to branch blocks joined through a hidden result slot, so only the selected
arm runs (**verified** by `ternary_does_not_advance_unselected_ta_state` and
`ternary_size_guard_skips_out_of_bounds_get`). Literal-only arms keep
`SELECT`. Test names below
refer to `crates/pinewasm_vm/src/tests.rs` unless noted.

| Construct                                        | Rule                                                                             | Status                                                                                                                                                                                                                        |
| ------------------------------------------------ | -------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `+ - * / %` with an `na` operand                 | result is `na`                                                                   | **verified**: `na_propagates_through_arithmetic_and_comparison` (float); int shares the same propagation path (`pop_int` returns `None` for `na`), pinned via history reads in `int_values_persist_in_user_slots_and_history` |
| unary `-` on `na`                                | `na`                                                                             | assumed (same code path as arithmetic; no dedicated test)                                                                                                                                                                     |
| float `/` by zero, float overflow                | non-finite raw result → `na`                                                     | **verified**: `division_by_zero_is_na`, `division_and_modulo_by_zero_are_na`                                                                                                                                                  |
| float `%`                                        | remainder with the sign of the dividend (assumed); `x % 0` → `na`                | **verified**: `modulo_int_and_float`, `division_and_modulo_by_zero_are_na`                                                                                                                                                    |
| int `/`                                          | float, keeps its fraction (`7 / 2` is 3.5); `/ 0` → `na`                       | **verified**: `int_division_keeps_its_fraction`, `division_and_modulo_by_zero_are_na`                                                                                                                                         |
| int `%`                                          | remainder with the sign of the dividend (assumed); `% 0` → `na`                  | **verified**: `modulo_int_and_float`, `division_and_modulo_by_zero_are_na`                                                                                                                                                    |
| int overflow (any op, incl. unary `-`)           | `na` (checked arithmetic; assumed)                                               | **verified**: `int_overflow_is_na`, `unary_minus_overflow_is_na`                                                                                                                                                              |
| mixed int/float arithmetic, comparison, equality | int side promoted to float (explicit compile-time coercion)                      | **verified**: `mixed_division_is_float`, `int_comparisons_and_mixed_comparisons`                                                                                                                                              |
| `< <= > >= == !=` with a missing numeric operand | result is `false` | VM: `missing_numeric_comparisons_are_false_in_v6`; native Wasm: `missing_numeric_comparisons_return_false_in_compiled_wasm`. All six operators, int/float and either/both operands missing. |
| `< <= > >=` on ints                              | exact i64 comparison                                                             | **verified**: `int_comparisons_and_mixed_comparisons`, `bar_index_is_int_and_supports_modulo`                                                                                                                                 |
| `== !=` with an `na` operand                     | result is `na`                                                                   | assumed (implemented in `CmpEq`/`CmpNe`; no dedicated test)                                                                                                                                                                   |
| `and`                                            | `false and x = false`; `true and true = true`; otherwise (`na` involved) `na`    | **verified**: `logic_follows_kleene_semantics` (`na and true = na`)                                                                                                                                                           |
| `or`                                             | `true or x = true`; `false or false = false`; otherwise (`na` involved) `na`     | **verified**: `logic_follows_kleene_semantics` (`na or true = true`)                                                                                                                                                          |
| `not na`                                         | `na`                                                                             | assumed                                                                                                                                                                                                                       |
| `if` condition is `na`                           | treated as false: the else branch (if any) runs                                  | **verified**: `na_propagates_through_arithmetic_and_comparison`, `logic_follows_kleene_semantics`                                                                                                                             |
| `?:` condition is `na`                           | treated as false: `SELECT` pushes the else value (assumed, consistent with `if`) | **verified**: `ternary_na_condition_selects_the_else_branch`                                                                                                                                                                  |
| plotting `na`                                    | the bar's plot cell is `null` in output                                          | **verified**: `core_example_produces_na_2_minus1`, `golden_close_difference_example`                                                                                                                                                 |
| non-finite market data                           | reads as `na`                                                                    | **verified**: `non_finite_market_data_reads_as_na`                                                                                                                                                                            |

The `and`/`or` rules are Kleene three-valued logic: `na` behaves as
"unknown", so an operand that already determines the result (`false`
for `and`, `true` for `or`) wins regardless of the other side.

### Pure builtins and na

Every `CALL_PURE` function propagates `na` — any `na` argument produces
an `na` result — with two deliberate exceptions whose purpose _is_ `na`:

- `na(x)` pushes a plain bool (`true` iff `x` is `na`), never `na`
  itself — **verified by test** `na_function_on_history_and_literals`.
- `nz(x, replacement)` pushes `x` unless it is `na`, else the
  replacement; the compiler materializes the typed default (`0`, `0.0`,
  `false`) when the replacement is omitted — **verified by test**
  `nz_replaces_na_per_type`.

Domain errors are not runtime errors: `math.sqrt(-1)`, `math.log(0)`,
`math.asin(2)`, and float overflow produce non-finite raw results that
normalize to `na` (`Value::from_f64`), and `math.abs(i64::MIN)` or a
`math.floor`/`math.ceil`/`math.round` result outside the i64 range is
`na` like any other checked-int overflow — **verified by tests**
`math_sqrt_pow_exp_log_hand_computed`, `math_trig_hand_computed`,
`math_abs_preserves_int_and_float`, `math_floor_ceil_hand_computed`
(`crates/pinewasm_vm/src/tests.rs`).

### `math.log` rounding

`math.log` returns the natural logarithm correctly rounded to the nearest
binary64 value, on every target: the interpreter, linked WebAssembly and AOT
artifacts all call the same `pinewasm_num` implementation
(`crates/pinewasm_num/src/ln.rs`). Two kernels compute it, and they never
disagree, because each returns only the correctly rounded value:

- A fast kernel reduces the argument exactly with a 128-entry table and
  evaluates the logarithm in binary64 arithmetic as an unevaluated sum
  `sum + tail`. The module documentation proves that its error is below
  `2^-52 * r^2 + 2^-84 * |w|`, where `r` (`|r| < 2^-7`) is the reduced
  argument and `w` the exact table term; that is at most about
  `2^-59 * |ln(x)|`. When `|tail|` plus this bound stays below half the gap
  to the next binary64 value, every value within the bound rounds to `sum`,
  so `sum` is the result.
- Otherwise an integer interval kernel decides. It evaluates the logarithm
  with up to 192 fractional bits, enough to round every binary64 argument.

The fast kernel decides more than 99.9% of the arguments in each tested
family; the interval kernel handles only those whose logarithm lies very close
to a rounding boundary. Tests in `ln.rs` recompute every table constant with
the interval kernel, measure the fast kernel's error against its bound, check
that every result it decides is the interval kernel's result, and require it
to decide at least 99.5% of each random input family.
`crates/pinewasm_num/tests/log_cost.rs` fails when `math.log` costs more than
four times the pinned `libm` logarithm in an optimized build.

### `math.random` determinism

The VM owns one deterministic PRNG per run; it consults no clock and no
OS entropy, so `run` stays a pure function of
`(program, market_data, options)`:

- Each executed `math.random` draw is
  `splitmix64(key)[n]` — the `n`-th output of a splitmix64 stream —
  where `n` is a per-run draw counter (incremented once per draw that
  produces a value) and `key` is the call's `seed` argument when given,
  else a fixed default seed baked into the VM.
- Same program + data + options → identical draws, byte for byte —
  **verified by tests** `deterministic_across_runs` (which includes
  seeded and unseeded `math.random` cases) and
  `math_random_is_deterministic_and_in_range`.
- The result is uniform in `[min, max)` (53-bit mantissa resolution);
  `na` bounds or an `na` seed produce `na` (na-propagation; the counter
  does not advance for an `na` draw).
- **Draw sequence:** all draws
  share one per-run counter, so adding or removing _any_ `math.random`
  call shifts the later draws of the same run; a seeded call keys its
  own stream but still consumes the shared counter. Determinism is the
  contract; the draw sequence is PineWASM's own.

## Plots

- One plot slot per syntactic `plot(...)` call site, allocated in source
  order at compile time; the optional constant-string second argument
  becomes the slot's `title` (**verified**:
  `plot_with_title_and_slot_allocation` in
  `crates/pinewasm_semantics/src/tests.rs`, plus the `plot_titles.pine`
  fixture).
- Per-bar default is `na`; the top-level `plot` call writes its series value
  to the current bar's cell (**verified**: `if_else_branches_choose_plot`).
- Output shape: `PlotSeries { slot, title, values }` with one value per resident
  output bar. One-shot runs expose committed bars and truncate to
  `bars_executed` on early abort. An [`IncrementalSession`](rust-api.md) also
  exposes its open forming realtime bar, so `values.len()` can be
  `bars_executed + 1` until that bar closes. A host that only needs that bar can
  take it as a per-tick sample instead of a full-lane snapshot, which is what
  keeps the cost of one realtime tick independent of the resident history.

### `indicator(...)` display parameters

`indicator(...)` accepts the parameter names listed in the coverage matrix, in the positions listed there.
Covered display values are const-folded during analysis
(`indicator_display_params_compile_and_max_counts_const_fold`). Full parameter
semantics remain tracked in the coverage matrix.
Most are display/metadata-only in this headless VM (a matrix note flags each):
`shorttitle`/`overlay`/`format`/`precision`/`scale`/`timeframe`/`timeframe_gaps`/
`explicit_plot_zorder`/`calc_bars_count`/`behind_chart`. `timeframe_gaps` is
accepted only with a `timeframe` argument. `dynamic_requests = false` keeps
`request.*()` calls out of local scopes, as it does for `strategy(...)` and
`library(...)`.
`calc_on_every_tick` belongs to `strategy(...)`, not `indicator(...)`, and is
rejected on indicators. The `IndicatorInfo.calc_on_every_tick` field is
kept as false for wire/API compatibility and does not gate execution.
Indicators re-execute when the host supplies realtime updates
(`varip_counts_ticks_var_counts_bars` covers the VM tick path).
The four **drawing count caps** are _honored_:
`max_lines_count`/`max_labels_count`/
`max_boxes_count`/`max_polylines_count` (const int `1..=500`, `E0219` otherwise)
thread HIR → MIR → CSIL (`ProgramMetadata.drawing_caps`) to the VM's
`DrawingRegistry`, where each **overrides** the default oldest-first live-object
GC cap (default ~50) for its kind. `linefill` has no count parameter and keeps
the default. **Verified**: `indicator_max_lines_count_overrides_the_gc_cap`
(`max_lines_count=100` + 120 lines keeps the newest 100).

### Table cells

A `table` drawing carries an out-of-line **2D cell grid** keyed by the table's
heap handle, stored on the `DrawingRegistry` (so it is snapshotted/restored with
the live registry and heap on realtime rollback). The grid is **sparse** — only
cells written by `table.cell(...)` / `table.cell_set_<attr>(...)` /
`table.merge_cells(...)` exist — and bounded by the table's declared
`columns × rows` (validated at `table.new`); a `(column, row)` outside those
bounds is a structured `CollectionIndexOutOfBounds` runtime error, and an `na`
table receiver is a `NaFieldAccess` error — never a panic. Each cell holds the
optional attributes `text`/`width`/`height`/`text_color`/`text_halign`/
`text_valign`/`text_size`/`bgcolor`/`tooltip`/`text_font_family`/
`text_formatting` plus a `merged` flag; `table.clear(start_column, start_row,
end_column?, end_row?)` removes an inclusive cell range (the end defaults to the
start). The final grid is
serialized under `RunOutcome.drawings.tables[i].cells` — a `{ column, row,
merged?, attrs }` list in `(column, row)` order, carrying only the attributes
that were written (the serde types in
[`crates/pinewasm_vm/src/lib.rs`](../crates/pinewasm_vm/src/lib.rs) define the
authoritative shape). **Verified**:
`table_cell_sets_text_and_bgcolor_serialized`,
`table_cell_set_text_mutates_existing_cell`, `table_clear_removes_a_cell_range`,
`table_merge_cells_marks_the_span`, and
`table_cell_out_of_range_is_structured_error_no_panic` in
`crates/pinewasm_vm/src/tests.rs`.

### Polyline vertices

A `polyline`'s `points` field holds an `array<chart.point>` handle, and
`Value::Ref` crosses the wire as `null` — hosts could not reconstruct the
vertices from the `fields` map alone. The serializer therefore inlines the
dereferenced points under `RunOutcome.drawings.polylines[i].points` — a
`{ index, time, price }` list in array order (`null` for `na`), alongside the
raw fields. **Verified**: `polyline_new_stores_points_array_handle` in
`crates/pinewasm_vm/src/tests.rs`.

### Drawing ids

Each serialized drawing carries an `id`: its registration ordinal, counted
from 0 in creation order with one counter for every drawing kind. It is not a
heap handle, so collections, user objects and `chart.point`s allocated around
a drawing leave no gap in it, and the interpreter and a compiled module report
the same id for the same drawing. An id is never reissued after a delete or an
eviction, and a rolled-back realtime tick issues the same ids again.

A member that refers to another object (`line1`, `line2`, `points`) is `null`
in `fields`. A linefill names its two lines under `lineRefs` by their drawing
ids; an entry is `null` when that line is `na`, deleted, or evicted.
**Verified**: `drawing_ids_count_drawings_and_ignore_other_heap_objects`,
`drawing_ids_are_one_sequence_across_kinds`,
`drawing_ids_stay_with_their_drawings_through_delete_and_eviction`,
`linefill_reference_to_a_deleted_line_is_none`, and
`realtime_ticks_reissue_the_rolled_back_drawing_id` in
`crates/pinewasm_vm/src/tests.rs`. See
local regression tests.

## Strategies: the broker emulator

A script declares itself a _strategy_ with `strategy("Title", ...)` instead of
`indicator("Title")`. The two are **mutually exclusive** top-level
declarations — a script that declares both is a compile error (`E0240`,
**verified**: `strategy_and_indicator_are_mutually_exclusive` in
`crates/pinewasm_semantics/src/tests.rs`). A duplicate `strategy(...)` is `E0213`.

The full `strategy(...)` parameter surface is accepted and **const-folded** during
semantic analysis into `StrategyInfo` metadata (**verified**:
`strategy_full_param_surface_const_folds`). The compiler threads it
HIR → MIR → CSIL (`ProgramMetadata.strategy`), and the VM surfaces a `strategy`
section in `RunOutcome` for strategy scripts only (`None` for indicators —
**verified**: `indicator_run_has_no_strategy_section`).

### Parameters: honored vs. metadata

The defaults in this table are PineWASM's own choices.

| Parameter                 | Default     | Status                                                                                                                 |
| ------------------------- | ----------- | ---------------------------------------------------------------------------------------------------------------------- |
| `initial_capital`         | `1_000_000` | **honored** — seeds the equity series                                                                                  |
| `default_qty_type`        | `"fixed"`   | honored (order sizing) — assumed enum `{fixed, cash, percent_of_equity}`                                               |
| `default_qty_value`       | `1.0`       | honored (order sizing)                                                                                                 |
| `commission_type`         | `"percent"` | honored — assumed enum `{percent, cash_per_contract, cash_per_order}`                                                  |
| `commission_value`        | `0.0`       | honored                                                                                                                |
| `slippage`                | `0`         | honored (ticks of adverse fill)                                                                                        |
| `pyramiding`              | `0`         | honored (concurrent-entry cap)                                                                                         |
| `process_orders_on_close` | `false`     | honored (same-bar-close fills)                                                                                         |
| `calc_on_every_tick`      | `false`     | metadata — the tick-vs-close behavior maps onto the `RunOptions.realtime` tick model (see the indicator section above) |
| `calc_on_order_fills`     | `false`     | metadata only                                                                                                          |
| `currency`                | `"NONE"`    | metadata only                                                                                                          |
| `margin_long`             | `0.0`       | metadata only                                                                                                          |
| `margin_short`            | `0.0`       | metadata only                                                                                                          |

The enum-like string parameters (`default_qty_type`, `commission_type`) are
validated against the assumed token sets above; an out-of-set token is
`E0219` (**verified**: `strategy_bad_qty_type_token_is_rejected`).
`initial_capital`/`default_qty_value` must be strictly positive (`E0219`,
**verified**: `strategy_negative_initial_capital_is_rejected`). These token
sets and the intrabar limit/stop fill-price ordering are marked **assumed**
until a test pins them.

### `strategy` section of `RunOutcome`

```text
strategy: {
    initial_capital,          # starting equity
    trades: [ { entry_id, entry_bar, entry_price,
                exit_bar, exit_price, qty, comment?, pnl } ],  # pnl net of entry+exit commission
    position: { size, avg_price },   # size signed: +long / -short / 0 flat
    equity: [ f64 per executed bar ],  # initial_capital + realized + open PnL
    net_profit,               # = sum(trade.pnl) = final equity - initial_capital
    open_profit,              # unrealized pnl of the open position
    # plus summary metrics: gross_profit, gross_loss, win_trades,
    # loss_trades, even_trades, closed_trades, max_drawdown, max_runup,
    # avg_trade, avg_winning_trade, avg_losing_trade, max_contracts_held_all.
    # crates/pinewasm_vm/src/lib.rs defines the authoritative StrategyOutcome DTO.
}
```

### Order execution: the broker emulator

The order primitives (`strategy.entry/order/exit/close/close_all/cancel/
cancel_all`) place orders into a per-run **broker** (`pinewasm_broker`)
that fills them against the bar feed. These order effects may appear in a
conditional branch; unlike them, plot-family calls are global-scope-only. Order
calls lower to the `ORDER` opcode (see docs/ir.md). Ordering per bar `N`:

1. **Fill phase 1** (before running `main` on bar `N`): orders placed on bar
   `N-1` are pending; they fill against bar `N`'s OHLC. Skipped under
   `process_orders_on_close`.
2. Run `main` on bar `N` (which may place new orders and cancel pending ones).
3. **Fill phase 2** (`process_orders_on_close` only): the orders `main` just
   placed fill at bar `N`'s **close**.
4. Record the bar's equity: `initial_capital + realized + open PnL` marked to
   the close.

#### Fill-rule assumptions table

Each rule states PineWASM's implemented behavior and is pinned by a
value-asserting test in `crates/pinewasm_vm/src/tests.rs`. A row that no test pins is marked **assumed**.

| Order type                        | Trigger                    | Fill price                                                                                                                               | Status                                                                                                                                 |
| --------------------------------- | -------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| Market                            | eligible bar               | bar `open` (or `close` under `process_orders_on_close`)                                                                                  | verified (`market_entry_fills_next_bar_open`)                                                                                          |
| Buy limit                         | `low <= limit`             | `min(open, limit)` — no wait if the open already satisfies it                                                                            | verified (`limit_fills_when_touched_and_not_when_untouched`)                                                                           |
| Sell limit                        | `high >= limit`            | `max(open, limit)`                                                                                                                       | verified (mirror of the buy case)                                                                                                      |
| Buy stop                          | `high >= stop`             | `max(open, stop)`                                                                                                                        | verified (`stop_fills_when_price_trades_through`)                                                                                      |
| Sell stop                         | `low <= stop`              | `min(open, stop)`                                                                                                                        | verified (mirror)                                                                                                                      |
| Exit bracket (both legs)          | either leg's price touched | the leg the assumed intrabar path reaches first                                                                                          | verified (`exit_bracket_profit_and_loss_legs`, `both_legs_touched_same_bar_uses_open_proximity_path`)                                  |
| Both limit **and** stop, same bar | both touched               | intrabar path `open→high→low→close` when the open is nearer the high, else `open→low→high→close`; whichever level the path reaches first | **assumed** — the single-order tie ordering is a PineWASM design choice; pinned by `both_legs_touched_same_bar_uses_open_proximity_path` |

#### Position, trades, equity, and the honored params

- **Position** is a FIFO stack of lots; `size` is the signed sum, `avg_price`
  the abs-quantity-weighted mean entry (**verified**:
  `unequal_quantity_lots_weight_avg_price_by_absolute_quantity`, which pins
  the quantity weighting across unequal pyramided lots).
- A **reversing** `strategy.entry` (opposite the open position) adds the open
  size to the order, so it fully closes the opposite and opens the new
  direction (**verified**: `reversing_entry_closes_opposite_and_opens_new`). A
  basic `strategy.order` instead applies its requested quantity first to the
  opposite position and opens only any remainder in the new direction
  (**verified**:
  `strategy_order_ignores_pyramiding_and_splits_reversal_quantity`).
- An **exit/close** is reduce-only — it never flips past flat.
- **Pyramiding** caps concurrent same-direction `strategy.entry` lots at
  `max(1, pyramiding)`; values `0` and `1` therefore allow one lot, while `2`
  allows two. The over-cap entry is dropped (**verified**:
  `pyramiding_zero_and_one_allow_one_entry_while_two_allows_two`).
  `strategy.order` ignores this cap (**verified**:
  `strategy_order_ignores_pyramiding_and_splits_reversal_quantity`).
- **Slippage** (`slippage` ticks × `mintick`) is applied adversely to every
  fill; **commission** (`percent` / `cash_per_contract` / `cash_per_order`) is
  charged on every fill. A closed trade's `pnl` is net of **both** its entry and
  exit commission, so `sum(trade.pnl)` equals the equity delta and
  `net_profit == final equity − initial_capital`. The opening commission reduces
  equity immediately at entry; its per-unit share is carried on the lot and
  attributed to each closing trade's `pnl` (**verified**:
  `commission_and_slippage_are_exact`).
- **Equity** is sampled once per bar at the close (**verified**:
  `equity_series_is_exact_over_a_feed`).
- `default_qty_type`/`default_qty_value` resolve an omitted `qty`
  (`fixed`/`cash`/`percent_of_equity`).

**Supported order management and realtime behavior:**

- `strategy.close(id, qty/qty_percent)` reduces only entries with the matching
  ID, including partial closes. `strategy.exit(..., from_entry=id)` creates
  per-trade exit children for matching entries and applies per-call quantity
  reservations, so independently reserved partial exits remain live after an
  earlier partial fill (**verified**:
  `strategy_close_closes_a_named_entry_with_partial_qty`,
  `separately_reserved_partial_exits_survive_each_others_fills`).
- `strategy.entry`/`strategy.order` support `strategy.oca.none`, `.cancel`, and
  `.reduce`; exit brackets use OCA-reduce behavior and accept named OCA groups
  (**verified**: `strategy_oca_cancel_none_and_reduce_apply_to_entry_order_groups`,
  `strategy_exit_named_oca_group_keeps_same_tick_peer_executable`).
- A `strategy.entry`/`strategy.order` carrying both `stop` and `limit` is a
  stop-limit order: crossing the stop activates a limit that can fill later on
  the same bar or a subsequent bar (**verified**:
  `stop_limit_activates_before_limit_and_persists_across_bars`).
- `strategy.exit` supports trailing exits using `trail_price` or `trail_points`
  for activation plus `trail_offset` for the ratcheting stop (**verified**:
  `strategy_exit_uses_profit_loss_and_trailing_comments_and_alerts`,
  `strategy_short_trailing_exit_ratchets_across_bars`).
- Strategies run through the realtime rollback model while the broker's orders,
  fills, and position persist across ticks. Both one-shot runs and incremental
  sessions execute against appended realtime bars (**verified**:
  `strategy_with_realtime_ticks_executes_against_the_appended_feed`,
  `incremental_session_preserves_strategy_orders_and_fills_across_requests`).

Indicators, including `request.security`, also run in realtime. The
`request.security` back-mapped memo is invalidated whenever a tick appends or
overwrites a bar, so an appended bar recomputes its higher-timeframe value
instead of reading `na` (**verified**:
`request_security_memo_is_invalidated_on_a_realtime_appended_bar`).

`time(timeframe)` and `time_close(timeframe)` keep their per-timeframe bucket
memo across ticks instead. A tick that appends a bar groups only that bar onto
the memo, because appending never moves an earlier bar's bucket, and a tick
that re-runs a bar reads the memo unchanged. The memo therefore matches a
regroup of the session's current feed, and apart from the exception below, an
appended bar reports what one run over the same bars reports and adds the
same charge however long the session runs (**verified**:
`incremental_session_reports_the_forming_bar_on_every_tick`,
`one_shot_realtime_appended_bars_match_the_batch_run`,
`a_session_streamed_to_its_bar_limit_charges_every_appended_bar_alike`,
`appending_one_bar_at_a_time_matches_a_full_regroup_of_each_feed_state`). The
memo applies the grouping rule of the resample's `bucket_index` kernel one
appended bar at a time instead of calling that Aeneas-extractable kernel, so
differential tests against the full resample (`resample::bucket_opens_tests`),
not the extracted kernel, hold the two groupings equal.

A feed supplied without a `tradingDay` lane stays without one while its ticks
omit `tradingDay`. The appended bar's `time_tradingday` is still derived from
its own time, and calendar (`W`/`M`) buckets keep reading each bar's close
date, as one run over the same bars does. So weekly and monthly bucket times,
including the ones `bars_back` reads for history bars, equal the batch values
on every tick, also on a symbol whose bars open and close on different
exchange dates, such as daily bars stamped 00:00 UTC on a `UTC-5` symbol, and
across weekend session gaps and forming bars (**verified**:
`the_first_append_keeps_calendar_buckets_on_the_batch_grouping`,
`streamed_daily_bars_across_week_and_month_boundaries_match_the_batch_run`).
Calendar buckets on the first append use each bar's close date, as the
batch grouping does.

The remaining exception is a tick that brings a `tradingDay` to a feed whose
history has none. That append materializes the lane for the history from each
bar's open date (the value `time_tradingday` reports), so the tick's exact
session day is kept, and regroups the calendar memos once over the whole feed
(`a_tick_that_brings_a_trading_day_materializes_the_lane`). A host that sends
`tradingDay` on ticks should send it for the history too.

### The `strategy.*` read surface

The broker state is readable back into the script through the `strategy.*`
getter series — the position (`strategy.position_size`/`position_avg_price`/
`position_entry_name`), money and margin (`strategy.equity`/`openprofit`/
`netprofit`/`grossprofit`/`grossloss`, `strategy.initial_capital`,
`strategy.margin_liquidation_price`, `strategy.opentrades.capital_held`), trade
counts and retention (`strategy.closedtrades`/`wintrades`/`losstrades`/
`eventrades`/`opentrades`, `strategy.closedtrades.first_index`), and the derived
metrics (`strategy.avg_trade`/`avg_winning_trade`/`avg_losing_trade`,
`strategy.max_drawdown`/`max_runup`/`max_contracts_held_all`). Percent forms are
available for the average, profit/loss, open-profit, drawdown, and runup metrics
(`strategy.*_percent`). Each is a `BuiltinSeries` variant
(`BuiltinSeries::is_strategy()`) lowered to the ordinary `READ_BUILTIN` opcode —
no new opcode — and reads off a **per-bar broker snapshot** frozen at the start
of the bar (after the prior bar's fills), so history
(`strategy.position_size[1]`) reads the prior bar's snapshot. Counts and
`closedtrades.first_index` push **int**, numeric account/position metrics push
**float**, and `position_entry_name` pushes **string**. Empty or inapplicable
numeric/string metrics surface as `na`. The getters are compile-gated to
`strategy()` scripts (`E0241`). The strategy getter surface is covered by
`complete_strategy_surface_compiles_to_validated_csil`; the established
position/PnL getters are also value-asserted by
`strategy_position_size_and_avg_price_series`,
`strategy_openprofit_and_equity_series`, and the metric-getter tests in
`crates/pinewasm_vm/src/tests.rs`. The coverage matrix distinguishes the newer
getters that currently have compiler-level evidence only.

### Per-trade accessors, risk rules, and account conversions

The broker **retains its closed-trade list** and its **open lots**, exposed to
the script through indexed accessor functions:

- `strategy.closedtrades.<field>(trade_num)` reads one closed trade;
  `strategy.opentrades.<field>(trade_num)` reads one open lot. `trade_num` is a
  `series int` index, **0 = the oldest** trade. An out-of-range (or negative, or
  `na`) index reads `na` — never a Rust panic. Fields: `entry_price`,
  `exit_price` (closed only), `size` (signed: + long, − short), `profit`,
  `commission` (the trade's pro-rated entry share plus its exit commission),
  `entry_bar_index`/`exit_bar_index`, `entry_time`/`exit_time` (the bar's epoch
  ms, mapped through the feed's time axis), `entry_id`/`exit_id`,
  `entry_comment`/`exit_comment`. Open-trade `profit` is marked to the current
  close; `max_runup`/`max_drawdown` track the best/worst per-lot excursion using
  each bar's high/low and are preserved onto closed trades. The corresponding
  `profit_percent`, `max_runup_percent`, and `max_drawdown_percent` accessors
  divide those account-currency amounts by entry notional and multiply by 100.
  Accessors are `CALL_PURE` tags (no new opcode) and, like the getters, are
  compile-gated to `strategy()` scripts (`E0241`). The non-percent fields are
  value-asserted by
  `strategy_closedtrades_accessors_read_each_closed_trade`,
  `strategy_closedtrades_commission_includes_entry_and_exit`,
  `strategy_closedtrades_track_max_runup_and_drawdown`,
  `strategy_opentrades_accessors_read_the_open_position`,
  `strategy_opentrades_track_profit_runup_and_drawdown`. Closed-trade percent
  values are also value-asserted; the open-trade percent call surface currently
  has compiler-level evidence only, as listed in the coverage matrix.
- `strategy.default_entry_qty(fill_price)` estimates the quantity that a new
  entry would use at `fill_price` when no explicit `qty` is supplied. It honors
  the strategy declaration's quantity mode/value, account-currency conversion,
  symbol `pointvalue`, and `mincontract` quantization; it does not place an
  order. **Verified:**
  `strategy_fx_rate_is_bar_aligned_across_sizing_pnl_commission_and_wire`.
- `strategy.risk.max_position_size(contracts)` is a compile-time-registered
  directive (an `ORDER` primitive, tag 7) evaluated each bar; it caps the
  broker's opened **same-direction** exposure, dropping the excess of an
  oversized entry. The last value registered on a bar holds.
  **Verified:** `strategy_risk_max_position_size_caps_an_oversized_entry`.
- `strategy.risk.allow_entry_in(value)` (tag 8) restricts the **entry
  direction**: `strategy.direction.long` blocks short entries,
  `strategy.direction.short` blocks long entries, `strategy.direction.all` (the
  default) allows both. A `strategy.entry` opening a blocked direction is
  dropped, while an entry in the blocked direction may still close an opposite
  open position instead of reversing into the disallowed direction; the last
  value on a bar holds.
  This is **enforced. Verified:**
  `strategy_risk_allow_entry_in_restricts_entry_direction`.
- `strategy.risk.max_drawdown(value, type)` (tag 9) is compile-registered and
  const-folded. The `type` argument is required and accepts
  `strategy.percent_of_equity` or `strategy.cash`; omitting it is `E0204`.
  The rule is **enforced** against the broker's peak marked equity. On breach,
  pending orders are cancelled, open lots are force-closed at the current
  close, and future entries are permanently halted. **Verified:**
  `strategy_risk_max_drawdown_cash_halts_and_flattens`,
  `strategy_risk_max_drawdown_percent_halts_and_flattens`,
  `strategy_risk_max_drawdown_requires_type_and_accepts_percent_and_cash_units`.
- `strategy.risk.max_intraday_loss(value, type)` (tag 10) is **enforced** against
  session-start marked equity. A breach cancels pending orders, force-closes open
  lots, and blocks new entries until the next session. **Verified:**
  `strategy_risk_max_intraday_loss_cash_halts_and_flattens_for_session`.
- `strategy.risk.max_intraday_filled_orders(count)` (tag 11) is **enforced** per
  session. Once the filled-order limit is reached, pending orders are cancelled
  and new entries are blocked until the next session. **Verified:**
  `strategy_risk_max_intraday_filled_orders_halts_until_next_session`.
- `strategy.risk.max_cons_loss_days(count, alert_message?)` (tag 12) is
  **enforced** at session boundaries. Once consecutive losing sessions reach the
  limit, future entries are halted for the rest of the run. **Verified:**
  `strategy_risk_max_cons_loss_days_permanently_halts_after_limit`.
- `//@strategy_alert_message=<text>` is a compiler-annotation directive parsed
  like `//@version` (a first-on-line `//@…` token that survives comment
  stripping) and stored as `StrategyInfo.alert_message` metadata (last one wins;
  empty when absent). It sets the strategy's default alert message — display-only
  in this headless VM. **Verified:**
  `strategy_alert_message_annotation_is_parsed_into_metadata`.
- `strategy.convert_to_account(value)` / `strategy.convert_to_symbol(value)` are
  currency-conversion interfaces. They are the **identity** unless a host rate
  table supplies a `currency_rate/<from>/<to>` entry (the same host-data pattern
  as `request.currency_rate`), where `<from>`/`<to>` are the symbol currency
  (`syminfo.currency`) and the account currency (`strategy(currency=)`). A
  currency token of `"NONE"` (the default) or equal from/to currencies is
  identity. **Verified:**
  `strategy_convert_to_account_is_identity_without_a_host_rate`.

## Resource limits and runtime errors

`RunOptions` (all enforced per run):

| Option                     | Default               | Enforcement                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| -------------------------- | --------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `max_instructions_per_bar` | 64,000,000            | one unit charged per executed instruction, with honest extra charges for work inside an instruction. `WINDOW_MEAN` and rescanning `CALL_TA` functions charge their window work; O(n) collection calls charge their element work; numeric `CALL_PURE` functions cost one unit, while string-bearing pure calls precharge the UTF-8 bytes inspected plus a conservative projected output size. `STR_CONCAT` likewise charges its projected result bytes. Exhaustion aborts with `InstructionLimitExceeded` before a projected string allocation — **verified**: `instruction_limit_is_enforced`, `window_mean_metering_charges_length_units`, `call_ta_metering_charges_the_window_cost`, `call_pure_costs_one_budget_unit`, `str_repeat_large_projection_exhausts_tiny_instruction_budget_before_allocating`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `max_instructions_per_run` | 1,000,000,000         | independent cumulative counter shared by all historical bars and realtime ticks. Every dispatch and honest extra charge decrements both counters before effects; exhaustion aborts with `RunInstructionLimitExceeded` — **verified**: `cumulative_instruction_budget_is_exact_across_bars` and the native and VM-backed AOT budget tests in `crates/pinewasm_aot/tests/differential.rs`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `max_string_bytes_per_run` | 33,554,432 (32 MiB)   | cumulative bytes reserved for runtime-created scalar strings from constant pushes, `CALL_PURE`/`STR_CONCAT`, collection clones/splits/joins, broker-retained order metadata, and textual regex compilation/cache storage. Projectable outputs reserve before allocation; reservations are conservative and are never returned, so allocation churn is bounded as well as retained output. Exceeding the cap aborts with `StringLimitExceeded`. `str.match` additionally limits patterns to 16 KiB, its memo to 64 entries and 256 KiB of textual keys/errors, regex backtracking to 100,000 steps, and delegated NFA/DFA construction to 256 KiB each — **verified**: `string_allocation_budget_is_cumulative_across_bars`, `pushing_owned_string_constants_is_charged_before_each_clone`, `array_new_string_charges_every_clone_without_double_charging_elements`, `str_split_rejects_100001_pieces_before_collecting_or_reserving_strings`, `broker_growth_and_retained_strings_are_reserved_before_order_effects`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `max_vm_bytes_per_run`     | 268,435,456 (256 MiB) | overflow-safe logical-payload preflight covers slot rings, written flags, TA state, output/result copies, stack/profile buffers, nested feed/run-option/program clones, strategy bar state, realtime traces, synthesized identity lanes, and incremental feed/output growth before large nested allocation. Script-selected security/pivot/time memo caches charge their projected storage and O(bars) construction before compute. Sparse table cells and log/alert/order growth reserve conservative cumulative bytes before effects, so clear/cancel churn cannot recycle the cap. The counter is not a process-RSS or allocator-capacity ceiling. Realtime rollback snapshot allocations—including duplicated slot/output cells, retained TA-engine payload, the heap arena (including collected slots), drawing/table state, and alert-dedup state—are outside this byte counter. Other collection/string/object and drawing limits constrain portions of the source state, but snapshot duplication can make peak process memory exceed `max_vm_bytes_per_run`. Exhaustion aborts with `VmMemoryLimitExceeded` — **verified**: `vm_preallocation_budget_accepts_exact_limit_and_rejects_one_byte_over`, `nested_market_data_and_run_options_are_rejected_before_vm_cloning`, `realtime_trace_is_included_in_the_preallocation_budget`, `realtime_preallocation_reserves_synthesized_identity_lanes`, `incremental_advance_uses_the_one_shot_realtime_memory_budget`, `broker_growth_and_retained_strings_are_reserved_before_order_effects`, `tampered_slot_ring_is_rejected_before_nested_allocation`, `script_selected_resample_caches_respect_the_vm_memory_budget`, `table_merge_cells_respects_cumulative_vm_memory_budget`, `event_stream_respects_cumulative_vm_memory_budget`                                                                |
| `max_stack_depth`          | 4,096                 | checked on every push; exceeding it aborts with `StackOverflow` — assumed (no dedicated test; guard is unconditional in `Vm::push`)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `max_bars`                 | none                  | caps a history-only bar loop at `min(max_bars, data bars)`. A nonempty realtime segment requires the complete supplied history and rejects a truncating value before execution — **verified**: `max_bars_limits_execution`, `realtime_rejects_a_truncated_historical_feed`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `max_collection_elements` | 100,000 | A budget of live collection elements (array items, matrix cells, map entries). Constructors and growth preflight the cap; explicit shrink returns budget. The VM collects unreachable objects at bar boundaries under arena or element pressure, marking only the slot cells that can still be read, the current persistent values, operand roots, and live drawings. A single bar can still exceed the cap before the next collection and abort with structured `CollectionLimitExceeded`. **Verified**: `heap_limit_bomb_is_structured_error_no_hang`, `unrelated_dynamic_window_does_not_pin_bar_local_arrays`, `collection_history_reader_survives_unrelated_dynamic_window_gc`, `persistent_collections_survive_collection`, and heap reserve/collect tests. |

**Loop metering.** Loops (`for`, `while`) introduce no special charge:
every instruction the loop executes — the header comparison, the body, and
the counter advance — is metered at its ordinary per-instruction cost, and
the loop's back-edge `JUMP` counts like any other. A loop that iterates `k`
times therefore charges roughly `k ×` its per-iteration instruction count
against both the per-bar budget (`max_instructions_per_bar`) and the run-wide
budget (`max_instructions_per_run`). The per-bar counter is the immediate
safety mechanism against non-terminating loops: a `while true` or simply a
very long loop aborts with a structured `InstructionLimitExceeded` once the
bar's budget is exhausted — never a hang. A `for` with a nonpositive explicit
step instead aborts immediately with a structured runtime error; zero
literal steps compile. **Verified**:
`infinite_while_true_hits_the_instruction_budget`
(`crates/pinewasm_vm/src/tests.rs`).

Expected validation, resource-limit, and execution failures use a structured
`RuntimeError`:

```text
RuntimeError { kind, message, bar_index?, instruction_offset?, span? }
```

For an ordinary runtime error, `span` comes from the optimized instruction's
source-map entry. A compiler-produced program that deletes CSIL instructions
also retains a versioned, bounded pre-deletion reference (code, exact spans,
and stack bound). Validation checks that this reference is executable and that
the deterministic optimization passes named by its version reproduce the
supplied target. Version 1 names the three-stage pipeline (dead literal stores,
single-use transfers, entry-prefix aliases) and
artifacts that carry it keep validating. Version 2 names the current
five-stage pipeline, which adds flow-sensitive aliases and jump threading.
The compiler emits the lowest version whose pipeline reproduces the program, so
a program the two new stages leave unchanged serializes exactly as before.
Validation rejects any other version. On a
fresh one-shot or prepared run that stops at a per-bar or run-wide
instruction limit, the VM replays that reference with the same market data,
inputs, and run options. It uses the reference error's source span only when
the error kind, bar, and committed plots, visuals, drawings, logs, alerts, and
strategy prefix match. The reported `instruction_offset` remains the actual
optimized CSIL offset. If replay does not match or cannot run, the span stays
at the optimized opcode's direct source origin. Realtime runs, persistent
incremental sessions, and AOT execution use that direct-origin fallback.
Retaining the reference increases serialized artifact size and VM memory
admission cost. Reference validation and deterministic reoptimization run on
every public `run_program` call; `PreparedProgram` validates once at admission.
Reference execution runs only after a candidate budget failure. The 16 MiB
reference admission cap counts reserved instruction/span vectors, nested
opcode strings and cell attributes, boxed secondary-expression nodes, and
JSON-encoded content. It is an accounting bound, not a process memory limit.
Strict CSIL decoders
must admit the optional `budget_reference` field before they can ingest newly
optimized artifacts. Programs with no deletion keep the same JSON shape.
A 1705-byte regression fixture is pinned by SHA-256. Current ordinary plots
include explicit default paint, so the corresponding Rust artifact is now
2445 bytes and includes `PushColor` and `PlotStyledF64`. Separate Rust tests
pin these current bytes, compare reference and optimized VM outputs, exercise
the instruction-budget boundary, and reject a forged reference. These finite
checks cover the styled artifact, which has no formal proof.

The reference's span authenticity depends on trusting the compiler-produced
source map: validation can check the rewrite and surviving spans, but cannot
recover erased source text from a serialized artifact alone.

All `RuntimeErrorKind`s:

| Kind                          | Raised when                                                                                                                                                                                                                                                                                                                                            |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `MalformedMarketData`         | OHLCV arrays have unequal lengths, or `mintick` is non-finite or not positive (checked before bar 0; `bar_index`/`span` are absent) — **verified**: `malformed_market_data_is_a_structured_error`, `invalid_mintick_is_a_structured_error`                                                                                                             |
| `InstructionLimitExceeded`    | a bar exceeds `max_instructions_per_bar`                                                                                                                                                                                                                                                                                                               |
| `RunInstructionLimitExceeded` | the complete run exceeds `max_instructions_per_run`                                                                                                                                                                                                                                                                                                    |
| `VmMemoryLimitExceeded`       | predictable or dynamic VM-owned state exceeds `max_vm_bytes_per_run`, or its checked size arithmetic overflows                                                                                                                                                                                                                                         |
| `StackOverflow`               | the operand stack would exceed `max_stack_depth`                                                                                                                                                                                                                                                                                                       |
| `StackUnderflow`              | a pop on an empty stack — unreachable after validation; kept as defense in depth                                                                                                                                                                                                                                                                       |
| `TypeMismatch`                | an operand has the wrong runtime type (float arithmetic on non-float, int arithmetic on non-int, comparison on non-numeric, logic on non-bool, plot of non-float, equality across types) — unreachable for validated programs except where the validator's abstract type is `Any`; **verified** on a corrupted program by `runtime_errors_carry_spans` |
| `InvalidProgram`              | structural failures the validator would have caught (constant/slot out of range, running past the end) — defense in depth for callers that skip `validate`                                                                                                                                                                                             |

The VM assumes callers run `pinewasm_csil::validate` first and re-checks the
cheap invariants at execution time. A panic is a defect rather than a supported
failure mode; the defensive error kinds above cover the malformed states that
the runtime explicitly checks.

## Determinism

Given the same `(program, market_data, options)`, `run` produces
identical output, byte for byte. The VM consults no clock, no OS random
source, and no environment; float arithmetic is IEEE-754 `f64` with
non-finite results normalized to `na`, int arithmetic is checked i64
with overflow normalized to `na`; iteration order is fixed by the
bytecode, ta-engine state evolves purely from the executed instruction
sequence and the input data, and `math.random` draws come from the VM's
own deterministic per-run PRNG (see above). **Verified by tests**
`deterministic_across_runs` (including `math.random` cases) and
`ta_engine_is_deterministic_across_runs` (recursive, windowed, and
condition-driven engine state), and
bounded generated malformed inputs are property-tested by
`compile_and_run_never_panics` in
`crates/pinewasm_compiler/tests/pipeline.rs`. That test is regression evidence,
not an exhaustive proof of totality over arbitrary inputs.

## Realtime tick execution

Execution over the supplied history runs each bar once and commits it. On top of that,
`RunOptions.realtime: Option<RealtimeSim>` appends an optional **realtime
segment** that replays bar-by-bar rollback semantics. It runs after the
historical loop (and is skipped if history aborted).

**Model.** `RealtimeSim { ticks: Vec<Tick> }`, where each
`Tick { bar_index, open/high/low/close/volume, time, is_bar_close }` is one
price update. A tick's `bar_index` must be either the last committed bar
(re-run it) or the append index `bars` (open a fresh realtime bar); any other
index is a structured `InvalidProgram` error (not a panic). While a bar is
open, the only valid index is that bar: a tick that would re-run or append
another bar before the open bar's `is_bar_close` tick is the same error, so an
open bar is never abandoned uncommitted. For each tick the
driver:

1. **Opens or re-runs the bar.** The first tick on a bar opens it: it appends
   a bar if `bar_index == bars`, carries persistent (`var`/`varip`) slots
   forward from the prior bar exactly like a historical bar, and takes a
   `StateSnapshot` of the bar's start. An append records the tick's `time`; if
   history used the synthetic axis, the VM first materializes those historical
   opens so the host timestamp remains observable. A realtime run must use the
   complete supplied history (`max_bars` may not truncate it). Every later tick
   on the same bar first **rolls back** to that snapshot.
2. **Overwrites OHLCV** for the bar with the tick's snapshot.
3. **Executes `main` once** with the barstate flags set for this tick.
4. **Commits on `is_bar_close`.** The current cells become permanent history;
   an appended bar now counts toward `bars_executed`, and the next tick opens
   a new bar. Non-close ticks leave the bar open for the next rollback.

This rollback rule: within a realtime bar every execution starts from the
same committed state, so a plot's committed value is always the value from the
bar's **closing** tick, and only that value lands in `RunOutcome.plots`. When
`RunOptions.realtime_trace` is set, `RunOutcome.realtime_trace:
Option<Vec<PlotTickSample>>` additionally records each tick's post-execution
plot values (the intrabar wiggle) — the committed history still holds only
bar-close values.

**`StateSnapshot` and cost.** The snapshot is an explicit struct holding the
current-bar cell of every **non-`varip`** user slot, the per-slot `written`
flags, every plot/color current-bar cell, every TA engine state, the bar-start
collection heap, live drawing registry, alert-dedup set, and the `math.random`
draw counter. Rollback restores these while retaining the heap graph named by
`varip` cells. Snapshot allocations include duplicated
slot/output cells, retained TA-engine payload, the heap arena (including
collected slots), drawing/table state, and alert-dedup state. They are outside
the deterministic `max_vm_bytes_per_run` byte counter. Other
collection/string/object and drawing limits constrain portions of the source
state, but snapshot duplication can make peak process memory exceed that
setting. There is no structural sharing.

**Log observations.** Recorded `log.*()` events remain in creation order,
including messages from non-closing realtime ticks. Rollback does not remove
those observations when it restores script state. One-shot realtime runs and
persistent incremental sessions use the same retention policy. The existing
alert rollback/dedup policy is separate and is not changed by log retention.
String-allocation and event-entry memory limits still constrain cumulative
log growth; a failed append leaves previously recorded events intact.
The regressions in `crates/pinewasm_vm/src/realtime_log_retention_tests.rs`
check complete event contents and ordering, state/drawing rollback, and
resource stops. The current log retention policy is covered by these tests
and compares complete outcomes.

**What rollback does not restore.** The run's cumulative meters are not part
of the snapshot: the runtime string bytes counted against
`max_string_bytes_per_run`, the VM byte reservations counted against
`max_vm_bytes_per_run`, and the instruction units counted against
`max_instructions_per_run` all stay charged for work a rolled-back tick did. This is
intentional: the meters bound allocation churn and total work over the whole
run, so repeating ticks on one realtime bar cannot reset them, and a long
realtime bar can exhaust a limit that the committed state alone would not
reach. **Verified by test**
`realtime_rollback_keeps_the_cumulative_run_meters`
(`crates/pinewasm_vm/src/tests.rs`).

**`varip`.** A `varip` slot is `persistent` (carry-forward + run-once init,
identical to `var`) but its current cell and `written` flag are
**excluded** from the slot snapshot/restore, so they survive rollback and
persist across ticks. For reference-valued cells, the runtime also retains the
reachable heap objects and rewrites moved handles as needed. The canonical
demonstration: over a 3-tick realtime bar a
`varip` counter counts ticks (3 increments) while a `var` counter counts bars
(1 net increment, because each tick rolls it back). `varip` is a slot
_attribute_ (`SlotDef::varip`), so no new opcodes were needed — reassignment,
history, and initialization all reuse the existing `var` machinery.

**`barstate.*`.** The seven predicates are `series bool` builtins wired to the
driver: `isfirst` (`bar_index == 0`), `islast` (the run's last bar — the
realtime bar when a segment exists, else the last historical bar), `ishistory`
/ `isrealtime` (committed bar vs. the open realtime bar), `isconfirmed` (the
closing execution of a bar — every historical bar, or a realtime bar's
`is_bar_close` tick), `isnew` (the first execution of a bar), and
`islastconfirmedhistory` (the last historical bar once a realtime segment
exists; **assumed** false for a history-only run — there is no realtime
frontier). With a realtime segment the last historical bar is
`islastconfirmedhistory`, not `islast` — the realtime frontier owns `islast`.

**Determinism.** The realtime path is as deterministic as the historical one:
the same tick script produces byte-identical `RunOutcome`s, and `math.random`
redraws identically after a rollback (the draw counter is snapshotted).

## Bounded user-slot history

The interpreter and AOT runtime store each user series slot in a bounded,
power-of-two ring rather than allocating one cell per chart bar. The ring size
is `(program.history_depth() + 1).next_power_of_two()`, where `history_depth()`
is the maximum of the effective `max_bars_back`, literal user-slot offsets, and
`WINDOW_MEAN` lengths minus one. Slot-storage memory is therefore independent
of chart length. Reads still implement `bar - offset` and return `na` before bar
0; realtime snapshots capture and restore only the current ring cell for each
non-`varip` slot. Ring reuse and realtime rollback are verified by
`slot_history_is_a_ring_not_one_cell_per_bar` and the realtime tests in
`crates/pinewasm_vm/src/tests.rs`.

## `max_bars_back`: the error contract

`max_bars_back` sets how far history can be read; a history reference beyond
that limit is a runtime error. `indicator(max_bars_back=N)` or a top-level
`max_bars_back(source, N)` call raises the limit for the references the
compiler cannot size by itself. A constant offset is not one of those: it
states its own depth, and the limit covers it without a declaration.

This VM applies a buffer-limit contract for history reads and uses that limit when sizing
its bounded per-slot history rings. One program-wide **effective ceiling**
stands for the buffer. The semantic analyzer settles it as the largest of:

- the default, `500`;
- every `indicator(max_bars_back=N)` metadata argument and top-level
  `max_bars_back(source, N)` call (both validated to `1..=5000`, `E0219`
  otherwise);
- every literal history offset in the script, up to the hard limit of `5000`.

Literal offsets are collected with their spans and counted in a **deferred
end-of-program pass**, as the declarations are (a `max_bars_back` call may
legally appear _after_ the offsets it covers). A literal offset therefore never needs a declaration: `close[501]`
in a script that declares nothing compiles, and raises the ceiling to `501`,
exactly as `max_bars_back=501` would. Only a literal offset past the hard
limit, which no buffer holds, is rejected, with **`E0233`**.

Dynamic series/simple-`int` offsets are checked at runtime against the same
ceiling: negative, `na`, or before-bar-0 offsets read `na`, but a non-negative
offset greater than the effective ceiling raises `HistoryBufferLimitExceeded`.
This is where a declaration matters. The compiler cannot see how far a
computed offset will reach, so a script that needs more than the default, or
more than its deepest literal offset, says so with `max_bars_back`.

The top-level `max_bars_back(source, N)` call is **effect-only at the call
site**: it emits no opcode and has no per-bar execution cost. It raises the
program's effective ceiling, which controls runtime dynamic-offset checks and
the user-slot ring capacity. It is `E0212` in expression position and `E0214`
inside a function body (a script-global setting cannot be per-instantiation),
mirroring `plot`.

Consequently a _literal_ offset past 5000 bars fails to compile (`E0233`),
while a too-deep _dynamic_ offset fails at runtime with
`HistoryBufferLimitExceeded`. Offsets within the ceiling that merely ask for a
bar before the feed starts still read `na`.

## `request.security` and secondary contexts

`request.security(symbol, timeframe, expression, gaps?, lookahead?,
ignore_invalid_symbol?, currency?, calc_bars_count?)` runs an
`expression` against either a **higher-timeframe (HTF) view of the chart's own
feed** or a host-provided external-symbol series. A host may provide full
same-symbol requested bars in `MarketData::request_bars`; when it does, their
OHLCV and history take precedence over chart-bar aggregation. Otherwise the
runtime resamples the chart feed. Because this VM is
network-free (see the invariants above), it does not fetch a second data feed
itself. For `syminfo.ticker`/`syminfo.tickerid`, it uses the honest **same-feed
resampling** design: the chart's [`MarketData`] is aggregated into HTF buckets
along the shared time axis, the `expression` is evaluated per HTF bucket over
the resampled bars, and the per-bucket results are mapped back onto the chart
bars honoring `gaps` and `lookahead`. For any other literal symbol, a plain
OHLCV/derived `expression` folds to the host selector
`security/<symbol>/<timeframe>/<source>` and reads `MarketData::request_data`
directly, defaulting to `na` when the host does not provide the series.
An unchanged `syminfo.basecurrency` alias followed by a literal suffix also
uses that external host table: the VM builds the symbol from the chart's base
currency for each run (for example, `EUR` plus `USD` reads `EURUSD`). A
reassigned or persistent alias is not treated as a fixed chart symbol.
The timeframe may also be `timeframe.period`, an empty string, or an unchanged
ordinary alias. Those forms use `MarketData::timeframe` at run time, including
in external-symbol host selectors. Direct string inputs, including
`input.timeframe()`, use the selected host override on each run; an empty
selection uses the chart timeframe. A const-qualified nonempty string alias
retains its literal selector. Input-stable ternaries can also choose a
timeframe from string or boolean inputs. The VM resolves the chosen timeframe
once per run, including for compound same-feed expressions and external host
selectors; it never freezes the default input choice at compile time.
Reassigned, persistent, and bar-varying timeframe selectors remain outside
the supported subset.

**Implementation.** The call site is wired end-to-end and value-asserted. The
analyzer (`analyze_security_call`) validates the argument surface and emits the
`READ_SECURITY { selector, source, htf_seconds, gaps_on, lookahead_on }` opcode
(see docs/ir.md); the VM (`Vm::read_security`) either resamples the feed once
per same-feed site (memoized per run), evaluates the requested builtin series
over each HTF bar, and back-maps the result via
`Resampled::map_to_chart_bars`, or reads the host table for an external
selector.

- **Resampling / OHLCV aggregation.** Fixed-width requests follow the
  exchange clock (`syminfo.timezone`), not the feed's first bar: a width
  under a day walks from local midnight and restarts every day, so `60`
  buckets open on the hour and `240` buckets at 00:00, 04:00, ... whatever
  bar the feed starts on. A width of a day or more, such as `7D`, walks from
  local midnight of the first bar's day. `W` requests use Monday
  calendar weeks; multiweek blocks restart at the first Monday of the year
  containing that week's Monday. Host `tradingDay` date identities are used
  without a second timezone conversion; the fallback uses nominal bar-close
  dates in the exchange timezone. Aggregation is **open = first, high = max,
  low = min, close = last,
  volume = sum** (`five_daily_bars_aggregate_into_one_weekly_bucket`).
- **`gaps`.** `barmerge.gaps_off` (default) repeats the last completed HTF value
  on intermediate chart bars. `barmerge.gaps_on` emits when a requested bucket
  first becomes visible: at confirmation with lookahead_off, or at the first
  chart bar of the bucket with lookahead_on. Other chart bars yield `na`
  (`security_close_weekly_gaps_off_lookahead_off_repeats_completed_htf_close`,
  `security_close_weekly_gaps_on_yields_na_between_closes`).
- **`lookahead`.** `barmerge.lookahead_off` (default) exposes only COMPLETED HTF
  buckets — no future leak; `barmerge.lookahead_on` exposes the still-forming
  bucket's final value on its earlier chart bars and therefore **REPAINTS**, as
  demonstrated on the boundary bar
  (`security_close_weekly_lookahead_on_repaints_forming_bucket`). Lookahead-on
  is a repaint hazard and is kept as a supported option.
- **HTF aggregation of derived series.** `hl2` (and the other derived averages)
  use the bucket's aggregated high/low/close, not a single chart bar
  (`security_hl2_weekly_aggregates_high_low_across_the_bucket`).
- **Validation.** The symbol must be a constant or input string, a recognized
  chart-derived base-currency suffix, or a chart-feed alias. The chart-feed aliases
  (`syminfo.ticker`/`syminfo.tickerid`; `security_ticker_alias_is_accepted`) use
  same-feed resampling. Any other literal symbol is a host-backed external
  selector (`security_external_symbol_reads_host_request_data`); a missing host
  table reads `na` (`security_external_symbol_defaults_to_na_without_host_data`).
  A requested timeframe lower than the chart's is a same-feed runtime error
  (`security_lower_timeframe_is_a_clean_runtime_error`). The `barmerge.*`
  constants are registered (`fa1_barmerge_constants_are_const_string`). `gaps`
  and `lookahead` are honored only for same-feed requests. A host-backed
  external read has no HTF bucketing on our side for them to affect, so
  requesting either as `_on` together with an external symbol is rejected
  (`E0242`, `security_external_gaps_on_is_e0242`,
  `security_external_lookahead_on_is_e0242`) rather than silently ignored;
  their `_off` defaults may still be passed explicitly
  (`security_external_explicit_off_flags_are_accepted`).
- **Secondary-context EMA.** `ta.ema` over a requested price source runs with
  independent smoothing state on the resampled bars; history such as
  `ta.ema(close, 2)[1]` reads the previous requested bar. Same-feed compounds
  resolve `input.source` and `input.int` length overrides once per run before
  resampling. Cross-script plot selections cannot be resampled and return an
  explicit runtime `Unsupported` error.
- **Trailing parameters.** `ignore_invalid_symbol` is type-checked as a
  boolean; a missing host series already evaluates to `na`. Because this
  network-free runtime has neither an FX conversion stage nor partial-feed
  evaluation, `currency` accepts only `currency.NONE` and `calc_bars_count`
  accepts only `0`. Other values are rejected with `E0242` instead of being
  silently ignored.

**Assumed simplifications** (this VM's own choices):

- **Host-provided external feeds.** Same-feed requests are derived by aggregating the
  chart feed, while external literal symbols are pure host-table reads. The VM
  does not synthesize or fetch external data; the host must provide the
  `security/<symbol>/<timeframe>/<source>` series.
- **Same-feed timeframe ordering is runtime data.** The analyzer validates a
  sane constant string or a direct string input. The VM checks
  same-feed requests against the actual `MarketData::timeframe` and
  raises `SecurityTimeframeBelowChart` if the request would aggregate down.
- **Calendar units differ from fixed widths.** `W` and `M` use calendar
  buckets; `7D` and `30D` remain fixed-width. Newly compiled constant `W`
  requests retain their unit in the existing serialized timeframe-expression
  lane; selected inputs and `timeframe.period` resolve that unit per run.
  Bytecode that stores seconds alone keeps its fixed-width meaning.
  Cache keys include calendar identity to prevent equal-duration requests
  from sharing results (`security_calendar_week_preserves_unit_and_distinct_memo_keys`).
- **Authoritative clocks control publication.** Supplied `requestTimeClose`
  lanes defer a requested bucket until its closing instant is reached by a
  chart close, or until the first supplied bar after that instant. A partial
  final bucket remains unconfirmed. OHLCV aggregation and publication indices
  are distinct. Without host clocks, publication retains the last-available-
  chart-bar fallback, which can publish sparse or partial buckets early.
  Calendar alignment is checked; publication timing is otherwise unverified.
- **`lookahead_on` repaints**; it is deterministic here but
  still leaks the forming bucket's close backward.

### Compound sub-expressions

A COMPOUND higher-timeframe `expression` — anything beyond a plain price series,
e.g. `ta.sma(close, 2)` or `(high + low) / 2` _inside_ `request.security` — is
**executed** by a _secondary context_ over the resampled bars, not rejected.
The analyzer lowers the checked sub-expression into a compact, serializable
`HtfExpr` tree carried by the `READ_SECURITY_EXPR` opcode. At run time the VM
resamples the feed once, walks the tree over the HTF bars in bucket order with
its **own per-context `ta.*` state** (independent of `main` — a length-2 SMA
tracks the _weekly_ closes 106/113/118, not the daily closes; asserted in
`security_compound_sma_executes_over_weekly_closes`), producing one value per
bucket, then maps back per chart bar through the same `gaps`/`lookahead`
back-mapping as the plain-series path. The whole back-mapped series is memoized
per site per run. `na` propagates through arithmetic/comparison; comparisons
yield `1.0`/`0.0` (the secondary context is float-typed).

**Verified subset.** OHLCV/derived price reads; int/float literals; unary neg;
binary arithmetic (`+ - * / %`) and comparison (`< <= > >= == !=`); the
slot-based `ta.sma`/`ta.change` and engine-backed `ta.ema`/`ta.hma`/`ta.vwma`, including direct
`input.source` and `input.int` selections on the same feed, plus unchanged
ordinary numeric declarations whose initializer is supported in the secondary
context; and requested-timeframe
history over any supported subtree through a dedicated `HtfExpr::History`
site. History offsets can be literals or compile-time-known non-negative
integer expressions and index HTF buckets, not chart bars. Direct integer-input
offsets, including unchanged aliases and forwarded helper parameters, use
`HtfExpr::HistoryInput` and bind to the selected value before evaluating each
run's secondary context. They preserve the dynamic-history buffer ceiling;
negative selections read `na`, following the current VM chart-history contract.
Input arithmetic and numeric casts bind per run and preserve fractional
division until index consumption; chart-varying offsets remain unsupported
(`security_input_history_binds_selected_offset_in_direct_and_nested_helpers`,
`input_history_round_trips_and_rejects_malformed_bindings`)
(`security_compound_history_offset_reads_prior_htf_bar`,
`security_compound_history_offset_applies_to_a_ta_result`,
`security_compound_history_offset_folds_a_simple_int_expression`). `gaps`/
`lookahead` are honored for compound expressions exactly as for plain series
(`security_compound_change_gaps_on_emits_only_on_bucket_close`,
`security_compound_sma_independent_state_lookahead_on`).

Float/boolean tuple expressions and tuple-returning helpers can be destructured from
`request.security`. Each component uses the same requested symbol, timeframe,
and merge modes, with its own secondary state and requested-timeframe history.
An inlined helper's immutable numeric local initializers are re-evaluated in
that context; its prefix is not run on chart bars. Bounded pure helper loops
can also expand into this requested-context expression: numeric accumulators,
constant `math.pow` weights, and the cardinality of a local `array.from` are
supported. Expansion is capped at 256 total loop iterations, 32,768 nodes,
and depth 96. Unknown bounds, effects, persistent state, and history on
loop-mutated locals fail closed. Boolean tuple components preserve their type
and use false values during missing history. The tuple shape
`[estimate[1], estimate[1] > estimate[2], estimate[1] < estimate[2]]`
is covered by `htf_pure_helper.rs`. External tuples use a
separate host selector for each component. Tests cover weekly open/close/SMA,
input length overrides, component order, history, and missing external lanes
(`security_tuple_function_reevaluates_each_component_in_requested_context`,
`security_literal_tuple_preserves_component_order_and_history`,
`security_tuple_external_components_use_distinct_host_lanes`). Tuple-returning
TA calls, components other than float/bool, persistent helper state, effectful prefixes,
and components outside the supported secondary-expression grammar still
produce `E0244`.

**Assumptions.** Boundary alignment, `1M`=30d, and `lookahead_on` repaint
carry over unchanged from the plain-series path above. Each moving average warms up
on higher-timeframe buckets according to its own TA engine rule; HMA needs its
outer weighted window after the source window fills. VWMA weights by resampled
bucket volume, not chart-bar volume.

**Remaining (→ `E0244`).** Other engine-backed `ta.*` (such as `ta.rma` and
`ta.rsi`), external-symbol compounds with input-selected source or length,
and constructs outside the subset above (ternary,
`nz`/`na`, and string ops) are rejected with `E0244` and a coverage-matrix
note.

## `request.*` external-data interfaces

The scalar `request.*` functions that source data external to the chart feed —
`request.dividends`, `request.splits`, `request.earnings`, `request.financial`,
`request.economic`, and `request.currency_rate` — are modeled as **interfaces**.
This network-free VM cannot fetch corporate actions, fundamentals, macro series,
or fx rates, so rather than omit them each accepts the canonical parameter
list, is typed as a `series float`, and lowers to the `READ_REQUEST` opcode.
The const-selector and host-table boundaries below are deliberate subsets, not
a claim that PineWASM implements each function's complete feed semantics.

**Selector folding.** The analyzer folds identifying arguments into a stable
key `member/arg0/arg1/…` (e.g. `financial/<TICKER>/<ITEM>/<PERIOD>`). The first
ticker/symbol argument of dividends, splits, earnings, and financial requests
may be `syminfo.ticker` or `syminfo.tickerid`, including unchanged aliases; the
VM resolves it against the chart's actual symbol for each run. Other
identifying arguments must be constant string literals; an unsupported dynamic
identifier is `E0242`. Positional and named arguments bind against each
function's parameter list in the coverage matrix, including maximum arity and scalar
types. Optional merge/invalid-symbol/currency/history-cap arguments are
type-checked but otherwise remain host concerns: this interface returns the
host value or `na` without fetching, merging, or converting data.

**Host-table backed.** At runtime the VM reads the per-bar value from the
optional table `MarketData::request_data: HashMap<String, Vec<f64>>`, keyed by
that selector. A **missing** key (the default — an empty map) makes the interface
return `na` every bar, so a script using `request.*` compiles and runs unchanged
without a data feed. A **present** series is read per bar (non-finite sample →
`na`; a series shorter than the bar count reads `na` past its end). Because the
table is a host input, a caller (or test) that _does_ have the data can supply it
and get real values back — the implemented interface is deterministic and
testable while data sourcing stays a host concern (matrix status: partial).

**`request.seed` (MISC).** `request.seed(source, symbol, expression,
ignore_invalid_symbol?, calc_bars_count?)` is a scalar
host-or-default interface reusing the `READ_REQUEST` path: the `source`/`symbol`
constant strings fold into a `seed/…` selector read from `request_data`
(host-or-default `na`); the required numeric `expression` and optional trailing
parameters are type-checked, but the value comes from the host table (no
second-`CsilFunction` execution — data sourcing is a host concern).

**`request.security_lower_tf` (MISC).** `request.security_lower_tf(symbol,
timeframe, expression, ignore_invalid_symbol?, currency?,
ignore_invalid_timeframe?, calc_bars_count?)` returns an `array<float>` of the intrabar values within
each chart bar via the `READ_SECURITY_LOWER_TF` opcode. The `symbol` must be the
chart's own feed and the `timeframe` a constant string (else `E0246`);
`expression` must be a plain OHLCV/derived builtin series read (mirroring
`request.security`'s plain-expression form). All three fold into a
`security_lower_tf/<symbol>/<timeframe>/<expression>` selector, so two calls on
the same symbol/timeframe with different expressions (e.g. `close` vs `volume`)
address distinct host-table entries instead of colliding on one selector. At
the call boundary, all seven parameters bind positionally or by name and
the four optional host controls are type-checked. They do not alter this
network-free host-table interface. At
runtime the VM builds a fresh float array from the optional table
`MarketData::security_lower_tf_data: HashMap<String, Vec<Vec<f64>>>` — a **missing**
key or uncovered bar (the default) yields an **empty** array, a **present** entry
its per-bar intrabar values (non-finite → `na`). This VM only aggregates _up_, so
lower-timeframe intrabar decomposition stays a host concern (matrix note).

The names and signatures are listed in the coverage matrix; host-dependent
behavior and edge cases remain matrix-noted assumptions.

## Order-flow footprint interface

`request.footprint()` and the `footprint.*`/`volume_row.*` accessors follow the
same host-or-default pattern, but over **object types** rather than a scalar.
`footprint` and `volume_row` are registered object types (like the drawing object
types): a `footprint` carries seven scalar order-flow aggregates (`buy_volume`,
`sell_volume`, `total_volume`, `delta`, `poc`, `vah`, `val`) plus a `rows` field
(`array<volume_row>`); a `volume_row` carries six scalars (`buy_volume`,
`sell_volume`, `total_volume`, `delta`, `up_price`, `down_price`) plus two
imbalance predicates (`has_buy_imbalance`, `has_sell_imbalance`).

**Object model.** `request.footprint(...)` lowers to the `BUILD_FOOTPRINT`
opcode. The analyzer folds the call's leading constant-string arguments into a
selector (`footprint/…`, mirroring `request.*`) and ignores trailing options
(e.g. `imbalance_percent`, `row_size`). At runtime the VM materializes the object
from the optional host table — allocating a `volume_row` heap object per host
row, the `rows` array, then the `footprint` object — and pushes its handle. Each
`footprint.*`/`volume_row.*` accessor is a plain field read (`GET_FIELD`) over
that object, so an **`na` receiver is a structured `NaFieldAccess` runtime
error** (never a panic), like the drawing object setters. Drawing getters
differ: on an `na` drawing id they return `na`.
`footprint.get_row_by_price(fp, price)` (the `FOOTPRINT_ROW_BY_PRICE` opcode)
scans the footprint's rows and returns the first `volume_row` whose
`[down_price, up_price]` range contains `price`, or `na`.

**Host-table backed.** The VM reads the per-bar sample from the optional table
`MarketData::footprint_data: HashMap<String, Vec<Option<HostFootprint>>>`, keyed
by the selector. A **missing** key or a `null`/absent bar entry (the default)
makes `request.footprint()` build an object whose scalar fields are all `na`,
whose imbalance flags are `false`, and whose `rows` array is empty — so a script
using the footprint surface compiles and runs unchanged without an order-flow
feed. A **present** `HostFootprint` (a per-bar sample with the seven scalars and
a list of `HostVolumeRow`s) fills the object's fields (non-finite scalar → `na`).
No real order-flow feed is involved — data sourcing stays a host concern. The
names and signatures are listed in the coverage matrix; host-dependent
behavior and unspecified edge cases remain matrix-noted assumptions.

### Market-entry funding

Market entries apply two funding checks before changing positions. Admission
compares the fill-price requirement with the last execution mark's equity,
rounded to the same four decimals used for percent sizing. If admission fails,
a reversal leaves the old position intact. The opening leg then checks exact
funds at that execution mark. This can admit a closing transaction while
discarding an opening leg at a rounding boundary. Conversely, an admitted
price gap can open exposure that immediately requires margin liquidation.
Explicit reductions and price-based orders retain their existing fill rules.

Synthetic VM regressions cover whole-contract gap rejection, fractional
percent-sizing boundaries, atomic entry versus order reductions, and immediate
liquidation after an admitted reversal. These synthetic tests check the behavior
described above.


### Boolean history normalization

Unavailable boolean history reads expose `false`, although the internal storage
uses a missing sentinel. Shared typed lowering applies boolean conversion after
static or dynamic reads, including compound-expression history. Evidence:
`unavailable_boolean_history_returns_false_in_v6` (VM) and
`unavailable_boolean_history_matches_compiled_wasm` (compiled Wasm), covering
user variables, built-ins, and compound expressions on the first three bars.
This does not prove unmatched conditional branches or collection defaults.


The current language regression indicator includes fractional-alpha colors and
therefore uses the VM fallback on ABI v6, whose native color payload stores
whole transparency percentages. Emitter inventory and tier snapshots record
that boundary explicitly. The split-handoff suite tests a named whole-alpha
variant to retain large-script native handoff coverage; it does not certify the
fractional-alpha regression indicator as native.
