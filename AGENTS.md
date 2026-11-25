# Repository workflow

- Work on the existing `feat/native-jit` branch for the current JIT work.
- Do not create local or remote branches unless the user explicitly asks.
- Do not create a branch for each experiment or benchmark.
- Do not create extra Git worktrees for experiments or benchmarks; use ordinary source snapshots without Git metadata.
- Use Make targets for relevant tasks when a Makefile exists.
- Use patch/edit tools, not Python, to edit files.
- Git commits must be unsigned, title-only conventional commits, without trailers.
- Code comments describe functionality, not rationale, and must not exceed 20% of implementation lines.
