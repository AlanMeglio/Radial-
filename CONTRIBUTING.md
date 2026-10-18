# Contributing

Small, focused pull requests are easiest to review. Before opening one:

```
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

- Pure logic (hit-testing, drag detection, format table, output naming) is unit tested; add a failing test first.
- Win32 and rendering code cannot be unit tested. `cargo run --release -- --preview out.png` renders a static frame
  of the menu, and `tests/e2e.ps1` drives a real drag and drop (it takes over the mouse for about three seconds).
- Keep the idle cost at zero: no polling loops or timers that run while no menu is open.
- Conventional Commit messages (`feat:`, `fix:`, `docs:`...).
