# Clear Display Manager

`clear-display-manager` is a premium GUI display manager greeter built using the `clear-ui` framework, leveraging Wayland via `smithay-client-toolkit` and GPU-accelerated graphics via `wgpu`. 

It integrates seamlessly with the rest of the **Clear OS** desktop ecosystem, offering a highly customized login greeter interface that transitions directly into the `clear-computing-environment-client` (River WM) or standard fallback sessions.

## Features

- **Premium Design Aesthetics**: Fully hardware-accelerated dark theme matching the design system of the Clear desktop environment.
- **Session Selector**: Interactive session cyclist allowing selection between the Wayland-based River window manager and a fallback Bash login shell.
- **Obfuscated Password Fields**: Dedicated custom password widget wrapper around `clear-ui` text inputs.
- **Focus Cycle Navigation**: Easily navigate fields using standard `Tab` focus switching keys.
- **Seamless Launch Integration**: Authenticates credentials and starts the session via `/home/lsgalante/Dropbox/Clear/clear-computing-environment-client/start-river.sh`.

## Architecture

- **Wayland Protocol Handling**: Managed using `smithay-client-toolkit` and `calloop` event dispatcher loop.
- **Rendering Engine**: `wgpu` with WGSL custom shaders and `glyphon` text atlas system.
- **Core GUI**: Designed as a layout grid inside a centered card frame containing:
  - Custom `LoginCard` container box
  - `TextBox` input fields
  - Cyclic `Button` session selector
  - `StatusLabel` validation indicator

## Running & Compiling

Build the project locally:

```bash
cargo build --release
```

Run in an existing Wayland environment (for testing/development):

```bash
cargo run
```
