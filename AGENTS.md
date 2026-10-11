# Quarry

A native desktop app on [ferrite-design](https://github.com/rwetz/ferrite-design)
(GPUI, Rust), started from the `workbench` template.

Before changing UI code, read ferrite-design's AGENTS.md — it is the
contract for how Ferrite apps are built (components, colors, type, motion,
and the mistakes that cost time). Short version:

- `use ferrite_design::prelude::*;` — every component comes from there.
- Colors only through `palette(cx)`; no hex values in views.
- Display type via `.display(Scale::X1, window)`, UPPERCASE; body via `.body(text::BASE)`.
- Controlled components: you own the value, the handler gets the new one.
- Check: `cargo build`, `cargo clippy`, and run it.

## Quarry-specific rules

- `src/search.rs` owns scanning, indexing and preview reads. It is an original
  implementation; do not add dependencies on private logscope source.
- Scan only explicit roots; preserve ignore rules and resource caps. File data
  never goes to network services. The catalog is an immutable snapshot in the UI.
- Search/scan/preview generation checks prevent stale background results.
- Do not describe a source build or CLI check as native visual verification.
- Release portable archives and Windows setup together, with SHA256SUMS.txt.
