# Fork provenance

- Upstream: https://github.com/iced-rs/winit
- Base: `48116469f15d3bc52b5e56bb516694be8882870d`
- Branch: `enzim`; crate names and upstream ancestry are unchanged.
- License: Apache-2.0. Upstream license files and notices are retained.
- Fork author and signer: Bernard, [enz1m](https://github.com/enz1m).

## Changes

- wayland: Expose owned parent objects
- wayland: Broker activation tokens
- wayland: Expose native toplevel tags
- wayland: Pipeline delayed frame callbacks
- ime: Support additional input purposes
- wayland: Support xdg popup windows
- window: Support transient dialogs
- wayland: Report touchpad gestures
- wayland: Report decoration and tiling state
- linux: Report Caps Lock modifiers
- wayland: Support drawing tablets
- wayland: Set window icons
- wayland: Restore window placement
- wayland: Coordinate popup grabs and replacement
- wayland: Position popups in parent geometry
- wayland: Retain frame shadow surfaces across transitions
- ime: Update surrounding text for input methods

The base is iced-rs/winit 0.30.13 already merged into the installed fork.
Upstream macOS formatting is retained. No new upstream upgrade is included.
