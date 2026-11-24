# Security policy

## Supported versions

Until the first tagged release, security fixes are made on the current `main`
branch only. After releases begin, the current `main` branch and the latest
tagged release are supported. Older releases are unsupported unless a
published advisory explicitly says otherwise.

## Reporting a vulnerability

Please report vulnerabilities privately to
[`business@ai-edge.io`](mailto:business@ai-edge.io?subject=PineWASM%20security%20report)
with the subject `PineWASM security report`. When this repository's Security
tab offers "Report a vulnerability", you can use that form instead; the email
route is always available. Do not open a public issue for a security problem.

Please allow coordinated disclosure while we investigate and prepare a fix.
We will acknowledge the report, keep you informed of material progress, and
credit you in the advisory unless you prefer otherwise. We will coordinate a
disclosure date with the reporter rather than publishing identifying or
technical details without notice.

Crashes found by adversarial or longer-running fuzz campaigns follow the same
private route until triage shows they are ordinary correctness bugs. The
minimization, regression-promotion, and CI-log rules are documented in
[docs/fuzzing.md](docs/fuzzing.md).

## Scope

**In scope — the compiler and VM resource limits.** PineWASM compiles and
runs untrusted scripts, so the VM's per-bar and cumulative instruction, stack,
bar, collection, string, and VM-owned memory limits are a real security
surface. A script that escapes them — unbounded
memory growth, a hang that outlives the instruction budget, a panic
reachable from script source, or an escape from the VM's value model — is a
vulnerability, as is anything that lets compiled output read or write beyond
the data the host handed to the run.

String-producing VM operations are charged for inspected/projected bytes and
share a cumulative per-run allocation budget (32 MiB by default). `str.match`
also enforces a 16 KiB pattern cap, bounded memo entries/text bytes, bounded
compiled-regex automata, and a finite backtracking limit. Limit failures are
structured runtime errors in both the interpreter and AOT host path.

`maxInstructionsPerRun` (one billion units by default) bounds repeated work
across historical bars and realtime ticks independently of the per-bar cap.
`maxVmBytesPerRun` (256 MiB) checks predictable slot-ring, TA, output,
profile, nested feed/run-option/program-clone, strategy, and realtime-trace
state before large allocation, then cumulatively charges sparse table cells
and log/alert/broker growth. Script-selected resample caches
(`request.security`, pivot, and time functions), collection
constructors/copies, and split/join operations project and reserve their
storage before materialization. Limits are checked before the corresponding
script-visible effect. The caller's already-existing `CsilProgram`,
`MarketData`, and `RunOptions` allocations remain owned by the embedding; the
limit covers the VM's projected clones and runtime growth, not memory the
caller allocated before invoking the VM.
Native AOT charges both instruction counters; an ABI-v6 `host.vm` site that
cannot preserve the two-counter decision exactly returns an explicit reroute
status. An embedding host must rerun that request on PineWASM's interpreter.
Every emitted native artifact declares exactly one wasm32 memory capped at no
more than 256 MiB. A consumer must validate that declaration and apply its own
output-copy limits before instantiation. Native and interpreter memory errors
are therefore independently bounded, but their representation sizes, failure
timing, and error classification are not claimed to be identical.

**In scope — untrusted programs, artifacts, and library sources.** Serialized
CSIL and Wasm are untrusted input. `run_program` revalidates caller-supplied
CSIL at execution. Prepared programs and incremental sessions validate at
admission and retain immutable code; later runs or ticks reuse that admitted
code with their documented state and budget rules. The low-level VM `run`
requires previously validated CSIL and is not an untrusted cache-admission API.
The AOT producer also validates programs before emission. Hosts that load foreign Wasm remain
responsible for validating bytes, enforcing the documented import/export
allow-list and memory maximum, and binding a hosted wrapper to the exact
validated program it delegates to. The same untrusted-input posture applies to
library source threaded into a multi-source compile.

The compiler accepts at most 128 caller-supplied libraries, each path is at
most 256 UTF-8 bytes, each source shares the 1 MiB source cap, and all library
sources together are capped at 8 MiB. The Wasm adapter checks those limits
before serialization; the compiler enforces them again. CSIL validation also
preflights reachable operand-stack heights and caps typed abstract
interpretation at 8,000,000 retained stack cells before allocating
per-instruction snapshots. Size-limit bypasses or pre-limit memory exhaustion
are security issues.

Host-reactive `input.*(active = ...)` and input-bound style expressions are
also untrusted artifact metadata. They are limited to 256 expression nodes, 64
levels of nesting, 32 call arguments, and 64 KiB of materialized UTF-8 string
data. Call nodes are restricted to a small cross-host-audited set (`na`, `nz`,
and simple numeric sign/extrema calls). Parsing, Unicode transformation, calendar,
transcendental, and regex calls should not be independently reinterpreted by a
JavaScript host: it may not reproduce every VM edge case or, for `str.match`,
the VM's finite backtracking budget. Unsupported expressions use the
compiler-provided safe default. Malformed or over-budget expression trees must
fail closed.

**In scope — the language server's workspace confinement.** Files discovered
by `pinewasm`, and library paths configured for it, must
resolve inside the canonical workspace root. Reading outside that root — or
returning a workspace edit that points outside it — is a vulnerability.

**Out of scope.** Denial of service against your own machine by scripts you
chose to run outside the resource limits you configured, and issues in
third-party dependencies with no PineWASM-specific impact (report those
upstream).
