# prtsc

A screen capture/recording CLI for Linux, driven entirely by the XDG
Desktop Portal - no window enumeration, no picker UI of its own. The
compositor's own screenshot/screen-share dialog (GNOME Shell, KDE, etc.)
handles source selection; `prtsc` just talks to the portal and PipeWire.

Also runs as an MCP server (`prtsc mcp`), exposing the same capture and
recording actions as tools for AI assistants/agents.

## Install

```sh
cargo install prtsc-mcp
```

This installs a `prtsc` binary (the crate is published as `prtsc-mcp` on
crates.io since the plain `prtsc` name was already taken).

## Requirements

- A Linux desktop with `xdg-desktop-portal` and a backend that implements
  the `Screenshot`/`ScreenCast` portal interfaces (GNOME, KDE, etc.).
- PipeWire, for the `record` command and `start_recording`/`stop_recording`
  MCP tools.
- A C++ toolchain at build time: `openh264` (video) and `fdk-aac` (audio)
  are both compiled from vendored source.

## CLI usage

```sh
# One-shot screenshot: opens the portal's screenshot picker, prints the
# saved file's location on success.
prtsc

# Record a screencast to an mp4 file until Ctrl-C/SIGTERM.
prtsc record [path]              # defaults to ./recording.mp4
prtsc record --audio [path]      # also captures desktop audio (the
                                  # default sink's monitor, not the mic)

# Run as an MCP server over stdio.
prtsc mcp
```

## MCP tools

Running `prtsc mcp` exposes three tools over stdio:

- **`capture`** - opens the screenshot picker and returns the saved
  file's location.
- **`start_recording`** (`path`? , `audio`?) - opens the screen-share
  picker and starts recording; blocks until recording has actually begun.
  Only one recording can be in progress at a time.
- **`stop_recording`** - stops the in-progress recording and returns the
  saved file's location.

## Why no window list

Earlier revisions had an in-app window picker (`winit` + `softbuffer` +
`ratatui`). It was dropped once the portal's own compositor-drawn picker
took over source selection - see `docs/implementation-plan.md` for the
full history, including the standalone `softbuffer-backend` crate that
rendering layer became.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
