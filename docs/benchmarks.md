# Performance measurements

## Cross-engine benchmark

The pinned run used four synthetic strategies: momentum, periodic, weighted, and
window. Each engine ran five 500-trial sweeps per workload on 4,096 generated daily
bars. All engines received the same data (seed 934), instrument settings, and
parameter sequence. The shared parameter sequence has its own seed.

| Engine | Build and execution route |
| --- | --- |
| PineWASM | 21.1.0, standalone WebAssembly |
| PineTS | 0.13.1, prepared JavaScript runtime from Pine source |
| PineForge | 1.10.0 at `59f98ba80e69a624f97ada11935e1bc973fd0e19`, C++ ports using its Pine source adapter |
| PyneCore | 6.12.1, Pyne ports using its local Python script runner |

PineForge v1.10.0 was the newest published engine release when these builds were pinned.
[PineForge v1.10.0 release](https://github.com/pineforge-4pass/pineforge-engine/releases/tag/v1.10.0)

PyneCore was the highest-star additional strategy runtime found when the baselines
were chosen. The star counts at that time were PineTS 934, PyneCore 263, and PineForge 205.
[PineTS](https://github.com/LuxAlgo/PineTS) · [PyneCore](https://github.com/PyneSys/pynecore) ·
[PineForge](https://github.com/pineforge-4pass/pineforge-engine)

Each worker starts with prepared code and immutable market data. Timings cover
fresh trial state, backtesting, and preparation of the common result from the engine's
plots, closed trades, and bar-close equity. Process startup, initial preparation,
worker transport, and correctness checks fall outside the timer. PineForge and
PyneCore use local ports, so the comparison measures their execution paths.

The correctness check compared nine metrics for each of the 40,000 timed trials
against the expected values for the same parameter sets.
All 160 full checks at the first and last parameter sets passed for numeric plots,
closed trades, bar-close equity, and the common metrics.

The table gives the median sum of worker time for one 500-trial sweep.

| Workload | PineWASM | PineTS | PineForge | PyneCore |
| --- | ---: | ---: | ---: | ---: |
| Momentum | 1.129 s | 14.213 s | 8.512 s | 20.399 s |
| Periodic | 0.706 s | 13.384 s | 10.857 s | 17.212 s |
| Weighted | 1.152 s | 15.167 s | 1.966 s | 26.614 s |
| Window | 0.935 s | 14.374 s | 4.359 s | 18.428 s |

For each workload, divide the baseline time by PineWASM's time in each paired repetition,
then take the median of the five ratios. The periodic workload gave the largest ratios
for all three baselines in this suite, **18.5× vs PineTS**, **15.4× vs PineForge**, and
**23.6× vs PyneCore**. The geometric means across the four workloads were **14.6×**,
**5.5×**, and **21.0×**, respectively.

These measurements describe one Apple M5 Max with 18 cores, macOS 26.6.2, Node.js
22.22.3, and CPython 3.12.0. The one-minute load average ranged from 10.9 to
26.4 at the start of each sweep. The figures apply to the tested prepared workloads.

## PineTS end-to-end replay

The replay includes worker startup and compilation. It used PineWASM 21.1.0 and
PineTS 0.13.1, with the shared parameter sequence and a pinned clock. It measured
**13.4× end to end** and **14.7× in summed worker time** across the four workloads.
The periodic workload measured **16.3× end to end**. All 20,000 metric checks and
80 endpoint checks passed.

PineTS's median elapsed time across the measured sweeps was 15.3 seconds.

## Longer histories

The window and periodic workloads also ran on 65,536 bars with the same first 30 parameter
sets for each engine. All five window repetitions passed, yielding **14.5× vs PineTS**,
**4.5× vs PineForge**, and **17.1× vs PyneCore**. Three periodic parameter sets in each
repetition crossed PineWASM's 9,000-order retention window. The stored trade rows
gave different trade-derived metrics, so the periodic workload is excluded from
the long-history ratios. The full run includes those trials and a separate
check of the whole-run counts.

## Generated-strategy replay

PineWASM measured a **7.2× geometric-mean speedup over PineForge v1.10.0** on
the 22 strategies whose closed trades and bar-close equity matched within tolerance.
Each strategy contributed the ratio of the median times from 20 prepared runs per
engine, following three warmups. The largest ratio was **29.5×** on
`007-bracket-tp-sl-oca-reduce-isolate-01`. The median times for that strategy were
26.1 ms for PineWASM and 769.6 ms for PineForge.

The replay used 100 strategy slots and a 53,929-bar feed.
PineForge completed all 100 slots, and PineWASM's standalone route completed 58.
The generated PineForge strategies use the native report interface. The [benchmark repository][benchmark-repo]
contains the raw samples, eligibility checks, and build and replay instructions.
[Pinned strategy assets](https://github.com/pineforge-4pass/pineforge-benchmarks-assets/tree/5759bf2813862886b0661df2f864381598039e4b)

## Reproduce the benchmarks

With Node.js 22 or newer and a clone of [pinewasm-benchmarks][benchmark-repo],
check out the revision used for this report and verify the saved measurements.

~~~sh
git clone https://github.com/artificial-intelligence-edge/pinewasm-benchmarks.git
cd pinewasm-benchmarks
git checkout --detach 61a0deef9237d2934cb012c372fc366317ad774f
node bench.mjs verify
~~~

The offline verifier checks the file hashes against the committed manifest, regenerates
the expected data, rechecks the stored results, and recomputes the timing ratios,
including 7.2× from the generated-strategy samples.

To rebuild the pinned engines and run the short check, use `node bench.mjs smoke`.
Use `node bench.mjs campaign` for the full four-engine run, `node bench.mjs pinets`
for the pinned-clock PineTS replay, or `node bench.mjs corpus` for the generated-strategy replay.
The [benchmark run guide](https://github.com/artificial-intelligence-edge/pinewasm-benchmarks/blob/61a0deef9237d2934cb012c372fc366317ad774f/results/current-engines-2026-11-07/README.md) lists the build prerequisites. Rebuilding builds the measured PineWASM 21.1.0 from source
revision `20d6304`, which is not in this public repository, so only `verify` runs
without a checkout that contains it. Pass that checkout with
`--pinewasm-source /path/to/PineWASM`.

## Verify the ratios

The values below are the summed worker times in milliseconds in paired repetition
order. The generated-strategy values are the per-strategy median times for the 22 matching
strategies. Run this with Python 3.10 or newer to recompute the README figures.

~~~python
from statistics import geometric_mean, median

totals = {
    "momentum": {
        "PineWASM": [1131.362, 1111.641, 1129.046, 1128.363, 1282.442],
        "PineTS": [13978.883, 13503.522, 14213.470, 14363.574, 19290.293],
        "PineForge": [8372.090, 9228.253, 8512.485, 8075.904, 12710.710],
        "PyneCore": [20399.284, 20651.786, 20266.119, 20042.428, 33322.254],
    },
    "periodic": {
        "PineWASM": [706.013, 704.261, 702.518, 721.509, 925.768],
        "PineTS": [14034.211, 13037.691, 12918.505, 13384.395, 16623.211],
        "PineForge": [12965.668, 10856.991, 10348.209, 10710.090, 14895.918],
        "PyneCore": [17211.870, 16638.136, 16466.219, 17283.584, 17844.693],
    },
    "weighted": {
        "PineWASM": [1164.658, 1136.220, 1151.644, 1120.393, 1391.174],
        "PineTS": [16151.263, 13731.076, 14333.493, 19555.574, 15166.804],
        "PineForge": [2152.731, 1934.367, 1966.071, 1940.612, 2200.109],
        "PyneCore": [28253.148, 28502.574, 26614.115, 25620.319, 25713.215],
    },
    "window": {
        "PineWASM": [929.689, 934.909, 920.766, 1233.783, 934.646],
        "PineTS": [14019.332, 16990.125, 14373.967, 19448.728, 13735.436],
        "PineForge": [4111.697, 4711.541, 4188.162, 7077.581, 4359.043],
        "PyneCore": [18427.502, 23822.081, 17386.906, 26943.542, 16748.805],
    },
}

for engine in ("PineTS", "PineForge", "PyneCore"):
    ratios = {
        workload: median(other / pine for other, pine in zip(row[engine], row["PineWASM"], strict=True))
        for workload, row in totals.items()
    }
    peak = max(ratios, key=ratios.get)
    print(f"{engine} peak {ratios[peak]:.1f}× on {peak}, geometric mean {geometric_mean(ratios.values()):.1f}×")

# Generated-strategy medians in milliseconds (PineWASM, PineForge).
corpus_medians = [
    (26.0535, 769.5873), (34.6142, 147.3114),
    (26.1055, 199.0165), (33.8686, 129.5621),
    (45.7633, 793.7959), (38.0625, 376.2540),
    (37.7684, 79.6939), (22.9011, 48.7788),
    (26.2186, 155.1538), (24.1456, 142.5895),
    (50.3258, 133.7021), (43.0193, 441.3195),
    (24.6036, 110.3723), (45.1958, 626.5070),
    (24.4111, 204.5764), (41.1716, 1018.3215),
    (36.9130, 292.2727), (58.4787, 707.6209),
    (47.8120, 363.7088), (22.8323, 109.4823),
    (23.1470, 90.5617), (15.8166, 260.7473),
]
corpus_ratios = [forge / wasm for wasm, forge in corpus_medians]
print(f"PineForge generated-strategy peak {max(corpus_ratios):.1f}×, geometric mean {geometric_mean(corpus_ratios):.1f}×")
~~~

## Local benchmark tooling

From the repository root, run either command for its usage:

```sh
cargo xtask benchmark-subject
cargo xtask benchmark
```

The [workloads](../benchmarks/workloads/) exercise arithmetic, history, state,
control flow, and collections with generated inputs. The
[subject manifest](../benchmarks/manifests/subject-tiers-v1.json) records
sampling, tolerances, and limitations for PineWASM-only measurements. It reports
cold process, cold runtime, compilation, cached execution, and incremental-bar
timings separately, plus artifact size and peak process memory. JavaScript/Wasm
binding, hosted AOT, and standalone AOT measurements are declared unimplemented
in that suite; another timing measurement is not a substitute.

Cross-engine measurements use the
[adapter-driven manifest](../benchmarks/manifests/node-indicators-v1.json).
Engine adapters are supplied through `PINEWASM_BENCHMARK_ADAPTER`, which must be
an absolute path outside this repository.

Keep generated data and reports in ignored `target/` directories. Record the
source revision, input data, settings, and host environment, and validate
outputs before comparing compilation, first use, or prepared execution.

[benchmark-repo]: https://github.com/artificial-intelligence-edge/pinewasm-benchmarks/tree/61a0deef9237d2934cb012c372fc366317ad774f
