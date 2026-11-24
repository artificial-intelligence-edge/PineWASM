---
name: Bug report
about: Something is broken — a crash, wrong output, or a compiler/tooling failure
title: ''
labels: bug
assignees: ''
---

## What happened

A clear description of the bug. If PineWASM's behavior for a Pine Script®
v6 source file differs from the published Pine Script® v6 documentation, please
use the **Divergence report** template instead — it captures what we need for
that case.
Do not disclose a suspected vulnerability here; follow `SECURITY.md` in the
repository root and report it privately to business@ai-edge.io.

## Reproduction

The smallest script or command that shows the problem:

```pine
//@version=6
indicator("Repro")
// ...
```

## Expected behavior

What you expected to happen.

## Actual behavior

What happened instead. Paste diagnostics, console output, or a screenshot
where relevant.
Remove credentials, personal information, private file paths, and proprietary
source from logs and screenshots before posting them.

## Environment

- PineWASM version / commit:
- Where it ran: native compiler / LSP binary / JavaScript-Wasm host
- Operating system and host runtime:

## Checklist

- [ ] I searched existing issues for this bug.
- [ ] This is not a security vulnerability requiring private disclosure.
- [ ] This report contains no source code I lack the rights to share.
