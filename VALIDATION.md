# Validation

Linux x86_64, Rust 1.95.0. Existing enzUI source versions are retained.

- 27 library tests pass.
- `cargo +1.95.0 clippy --locked --lib -- -D warnings` passes.
- Native mock-compositor popup-grab test passes.
- Native mock-compositor shadow lifecycle test passes.
- 185 Rust files match the installed fork after removing modification notices;
  the upstream macOS source has harmless whitespace/trailing-comma differences.
- Nightly formatting check reports existing source style differences; no
  unrelated formatting changes were folded into the patches.

Commands use a shared build cache, `CARGO_INCREMENTAL=0` and
`CARGO_PROFILE_DEV_DEBUG=0`:

```sh
cargo +1.95.0 test --lib --test wayland_window_shadow --test wayland_popup_grab
```

Source provenance, licenses, upstream ancestry and every fork commit signature
are checked for publication. These checks do not qualify untested platforms.
