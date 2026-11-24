---
name: Divergence report
about: PineWASM's behavior differs from the published Pine Script® v6 documentation
title: 'divergence: '
labels: divergence
assignees: ''
---

<!--
Divergence reports record where PineWASM's behavior differs from the documented
Pine Script® v6 language. The ideal report is a minimal script plus a citation
into the published language documentation — small enough that a failing test
can be written directly from it.
-->

Do not disclose a suspected vulnerability here. Follow `SECURITY.md` in the
repository root and report it privately to business@ai-edge.io.
Remove credentials, personal information, private file
paths, and proprietary source from any output you share.

## Script

The smallest script that shows the divergence:

```pine
//@version=6
indicator("Divergence repro")
// ...
```

Input data, if the behavior depends on it (bar values, timeframe, inputs).

## Expected behavior, per the language documentation

What the published Pine Script® v6 documentation says should happen.
Please cite the specific section or builtin entry (a link or a section name
— describe the documented behavior in your own words rather than pasting
the documentation's prose).

## Actual PineWASM output

What PineWASM does instead: plot values, diagnostics (with stable codes
such as `E0210` or `E0254`), runtime errors, or compile failures. Paste the
actual output.

## Where it ran

- PineWASM version / commit:
- Engine: native compiler / LSP binary / JavaScript-Wasm host

## Checklist

- [ ] I checked `docs/pine-v6-coverage.md` in the repository
      and the divergence is not already listed as a known one.
- [ ] This report contains no copied proprietary source code or
      documentation I lack the rights to share.
- [ ] This is not a security vulnerability requiring private disclosure.
