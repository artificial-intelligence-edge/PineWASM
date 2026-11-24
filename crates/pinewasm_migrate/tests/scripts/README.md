# Full-length migration scripts

Every `.v5.pine` file in this directory was written by hand for PineWASM's
migration tests. They cover common kinds of indicators, strategies
and libraries (dashboards,
multi-timeframe requests, pyramiding and trailing-stop strategies, drawings,
alerts, collections) so that each one exercises many migration rules at once,
the way a real script does.

Each script `name` has three files:

| File | Content |
| --- | --- |
| `name.v5.pine` | The input. |
| `name.v6.pine` | The exact output of `pinewasm migrate` with default options, reviewed by hand against [docs/migration.md](../../../../docs/migration.md). |
| `name.findings` | One line per reported finding: source line, rule, class, severity, and whether the rewrite was applied or left for review. |

The `scripts` test target (`tests/scripts.rs`) checks every pair. To
regenerate the expected files after an intentional change, run:

```sh
PINEWASM_BLESS=1 cargo test -p pinewasm_migrate --test scripts
```

Then review every changed `.v6.pine` and `.findings` file against
[docs/migration.md](../../../../docs/migration.md) before committing it. A
regenerated file is not accepted until that review is done.
