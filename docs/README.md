# Documentation

The root [README](../README.md) is the generated feature overview. Its
[source template](readme.template.md) intentionally uses links relative to
the **generated root README**, not to `docs/`; see
[generation and replay](development/readme-generation.md) before editing it.

- [Architecture](architecture/dbgjs-architecture-presentation.md):
  [data model](architecture/debugger-data-model-overview.md),
  [CLI model](architecture/cli-design.md), [RPC contracts](architecture/contracts.md),
  [context identity](architecture/context-identity.md), and
  [value rendering](architecture/value-rendering.md).
- [Development](development/ci.md): CI, README generation, and the
  [coverage benchmark](development/vscode-coverage-benchmark.md).
- [Walkthroughs](walkthroughs/vscode.md): VS Code, Node, website, and
  [VS Code process debugging](walkthroughs/debugging-vscode-processes.md).
- [Investigations](investigations/): dated debugging reports and speculative
  designs. Their commands, measurements, and proposals are historical, **not
  guarantees about the current CLI or approved feature work**.

The [GitHub issues](https://github.com/hediet/dbgjs/issues) are the backlog.
Open issues [#9](https://github.com/hediet/dbgjs/issues/9),
[#11](https://github.com/hediet/dbgjs/issues/11),
[#12](https://github.com/hediet/dbgjs/issues/12), and
[#18](https://github.com/hediet/dbgjs/issues/18) cover earlier concrete
requests; [#21](https://github.com/hediet/dbgjs/issues/21) through
[#27](https://github.com/hediet/dbgjs/issues/27) retain the remaining
architectural work without mirroring a checklist here.
For superseded roadmaps and task notes, consult their
[historical revision](https://github.com/hediet/dbgjs/tree/44cdfc0d59f4112c85c24e8b744f8bec96c69274/docs/todo).
