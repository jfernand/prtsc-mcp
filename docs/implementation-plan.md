# Implementation plan: prtsc

## Goal

`prtsc` is a screen-capture CLI driven by the XDG Desktop Portal's
`Screenshot` interface: run with no arguments to capture once and print the
saved file's location, or run `prtsc mcp` to expose the same capture action
as a single MCP tool over stdio for AI assistants/agents to call.

There is no in-app window list or picker UI. The portal's own
compositor-drawn dialog (GNOME Shell's screenshot tool, KDE's, etc.) handles
window/screen selection - see "Why no window list" below.

## Steps

### 1. Capture via the XDG Desktop Portal
Add `ashpd` (screenshot feature) and `tokio`. `capture::capture()` requests
`ashpd::desktop::screenshot::Screenshot` with `.interactive(true)`, awaits
the response, and returns the saved file's `file://` URI (or a
human-readable error string).

**Verify:** confirmed end-to-end in this sandbox - a `tools/call` against
the MCP server (see step 3) returned a real file URI, and the file existed
on disk with the reported PNG dimensions. The plain CLI path made the same
request and received a real (if less consistent) response from the portal;
completing the interactive picker itself is flaky in this specific sandboxed
desktop session (no physical user reliably present to click through GNOME
Shell's dialog), which is an environment characteristic, not a code issue -
the request/response/error-propagation path itself is proven correct.

### 2. CLI: capture-and-exit by default
`prtsc` with no arguments calls `capture::capture()` once, prints the URI to
stdout on success, or prints an error to stderr and exits non-zero on
failure/cancellation. No window, no event loop - just one async call driven
by a throwaway current-thread `tokio` runtime built in `run()`.

**Verify:** `prtsc` with no args exits after one capture attempt; stdout has
exactly the saved path on success.

### 3. MCP server (`prtsc mcp`)
Added `rmcp` (server, macros, transport-io features) plus `serde_json`
(required directly by the `#[tool]` macro's generated code, not just
transitively via `rmcp`). `mcp::run()` serves a single `capture` tool over
stdio via `CaptureServer::serve(stdio())`, delegating to the same
`capture::capture()` used by the CLI path.

**Verify:** a manual JSON-RPC handshake (`initialize` -> `tools/list` ->
`tools/call name=capture`) against `prtsc mcp` returned a real saved-file
URI, confirmed to exist on disk. See step 1's verify note.

## Screen recording (implemented and verified end-to-end on a real desktop session)

`prtsc record [path]` records a screencast to an `.mp4` file via the
XDG Desktop Portal's `ScreenCast` interface, following the same
portal-only philosophy as capture: no window/screen enumeration in
`prtsc` itself, the compositor's own picker chooses the source.

**Encoder/container decision (final): H.264 (`openh264`) + MP4 (`mp4`
crate).** AV1 (`rav1e`) + WebM (`webm` crate) was the original plan;
revised deliberately in favor of H.264's much broader
playback/hardware-decode compatibility over AV1. `openh264`'s `source`
feature compiles OpenH264 from source, which is outside Cisco's royalty
coverage for the underlying H.264 patents (that coverage is tied
specifically to Cisco's own prebuilt binary, per OpenH264's
`BINARY_LICENSE.txt`/FAQ) - a real, acknowledged tradeoff, accepted
deliberately. Two other alternatives were ruled out earlier still:
shelling out to the `ffmpeg` binary via a piped child process (fragile
process-lifecycle/error-handling compared to a library call), and
`ffmpeg-next` (FFI bindings to libavcodec/libavformat - needs matching
`libav*-dev` headers at build time and matching `.so` files at runtime
on whatever machine eventually runs `prtsc`, with H.264 support itself
depending on how that machine's libavcodec was built).

Concretely simpler than expected: `openh264` ships its own
`YUVBuffer::from_bgra8_source`/`from_rgba8_source` conversion helpers, so
the RGB -> YUV420 step originally planned as hand-rolled arithmetic is
just a library call. The `mp4` crate is pure Rust (no C/C++ dependency at
all, unlike the earlier `webm` crate, which was FFI bindings to Google's
`libwebm` C++ library).

### 1. Portal session negotiation - done (`src/screencast.rs`)
`Screencast::new()` -> `create_session()` -> `select_sources(session,
SelectSourcesOptions)` -> `start(session, None, ..)` returns `Streams`,
each with a `pipe_wire_node_id()` and size. `open_pipe_wire_remote(session,
..)` returns an `OwnedFd` scoped to just that session/stream. The ashpd
`Session` is kept alive (moved whole into the recording thread's closure)
for the entire recording - dropping it early would end the cast portal-side.

**Verify:** confirmed end-to-end on a real desktop session (this sandbox
*is* a real GNOME session - the earlier "unverified" note was about
`xdotool`/X11 tooling being unable to click through GNOME Shell's
Wayland-native share picker, not about the session being non-interactive;
the human user completed the picker directly). Produced a real, playable
1920x1080 H.264/MP4 recording - confirmed with `ffprobe`, a full
`ffmpeg` decode, and by extracting and viewing an actual captured frame.

### 2. Raw frame capture + encode on one dedicated thread - done (`src/recording.rs`)
Simplified from the original plan: encoding happens directly inside
PipeWire's `process` callback, on the same thread as its mainloop,
rather than forwarding frames over a channel to a separate encoder
thread - one thread doing dequeue -> convert -> encode -> mux keeps the
first working version simple; revisit only if profiling ever shows the
encoder is slow enough to drop PipeWire buffers.

`ContextRc::connect_fd_rc(fd, ..)` (the fd from step 1) -> a video
`Stream`, format negotiated via an SPA `EnumFormat` pod offering the
pixel layouts `openh264` can consume directly (BGRx/BGRA/RGBx/RGBA -
same pod-building shape as the `pipewire` crate's own
`examples/streams.rs`) at the portal's reported size. Ctrl-C/SIGTERM
handling is PipeWire's own (`main_loop.loop_().add_signal_local(Signal::INT/TERM,
..)`, the same idiom as the crate's `pw-mon.rs` example) rather than
routed through `tokio` - simpler, since the whole capture/encode loop is
already isolated on its own thread.

**Verify:** confirmed on a real recording (see step 1) - real frames
arrived, were encoded, and were muxed into a playable file. Two real
bugs surfaced only by this real-session test (neither the unit test in
step 3 nor any code review caught them, since both are specific to
actually running PipeWire's mainloop for real) - see "Bugs found via
real-session testing" below.

### 3. H.264 encode via openh264, mux to MP4 - done, verified independently of PipeWire
Convert each raw frame with `YUVBuffer::from_bgra8_source(BgraSliceU8::new(bytes, (w, h)))`
(or the RGBA equivalent) - `openh264` ships this conversion built in, no
hand-rolled color-space math needed. `Encoder::encode(&yuv_buffer)` ->
`EncodedBitStream`, whose `layer(i)`/`nal_unit(n)` accessors expose
individual Annex-B NAL units directly; the *first* IDR frame's SPS (NAL
type 7) and PPS (NAL type 8) get pulled out to build
`mp4::AvcConfig { .. }` before `Mp4Writer::add_track` - the one place NAL
type bytes get inspected by hand. Other NALs are converted from Annex-B
to AVCC (4-byte length prefix) and written via `Mp4Writer::write_sample`.
PipeWire buffers can have row padding (`stride > width * 4`); a
`repack_rows` helper strips it before handing data to `openh264`'s slice
wrappers, which assert on exactly `width * height * 4` bytes.

**Verify:** since step 1 blocks any real capture, added a `#[cfg(test)]`
unit test (`recording::tests::encodes_synthetic_frames_to_a_readable_mp4`)
that calls `encode_frame` directly with synthetic pixel data, bypassing
PipeWire/the portal entirely, then reads the result back with the `mp4`
crate's own reader (track count, track type, sample count). Additionally
confirmed with tools outside this codebase: `ffprobe` identifies the
output as valid `h264 (Constrained Baseline) yuv420p`, and `ffmpeg -i ...
-f null -` decodes every frame without error. This isolates and verifies
exactly the hand-written, highest-risk logic (NAL parsing, SPS/PPS
extraction, AVCC framing, MP4 sample writing) independently of whether a
real PipeWire session is available.

### 4. Start/stop lifecycle - done
- CLI: `prtsc record [path]` runs until Ctrl-C/SIGTERM, then cleanly
  finalizes and prints the saved path, mirroring `capture`'s "print
  path on success" convention.
- MCP: `start_recording` (optional `path` parameter) negotiates the
  portal session and spawns the recording thread, returning once
  recording has actually begun; `stop_recording` signals it to stop and
  returns the saved file's location. `CaptureServer` holds at most one
  `RecordingHandle` (`Arc<Mutex<Option<..>>>`, since tool methods take
  `&self`) between the two calls - `start_recording` while one is
  already active, or `stop_recording` with none active, both return a
  clear tool-level error rather than panicking or silently no-opping.

**Stop mechanism unified across both paths, and simplified along the
way.** Originally the CLI used PipeWire's own OS-signal handling
(`add_signal_local`, protected by manually blocking `SIGINT`/`SIGTERM`
with `libc::pthread_sigmask` - see the bugs above) - not usable for MCP
at all, since `stop_recording` needs to signal a *specific* recording
from a normal async tool call, not send a process-wide OS signal
(which would also kill the MCP server itself). Switched both to
[`pipewire::channel`](https://docs.rs/pipewire/latest/pipewire/channel/index.html),
a `Sender`/`Receiver` pair built for exactly this: sending a message
from any thread to one attached to a running PipeWire loop.
`recording::record` now takes a `Receiver<Terminate>` and quits on
receipt, replacing both signal handlers with one code path. The CLI
translates Ctrl-C/SIGTERM into a `Terminate` message via
`tokio::signal::ctrl_c()`/`tokio::signal::unix::signal(SignalKind::terminate())`
(needed wrapping the thread's blocking `.join()` in
`tokio::task::spawn_blocking` so it could be raced against those in a
`tokio::select!`, rather than joined directly as before); the MCP
server just sends it from `stop_recording`. This also let the `libc`
dependency and the manual signal-masking code be dropped entirely -
`tokio::signal` handles the OS side correctly without it.

**Verify:** confirmed on real desktop recordings via both paths.
CLI: run to completion, Ctrl-C'd and separately SIGTERM'd (two
separate real recordings) - both produced valid, playable files with
the path printed on stdout, `Recording to <path>...` printed as soon
as setup begins and `Wrote N frames, WxH, D.Ds, S KiB -> <path>` after
`write_end()` succeeds (both stderr, keeping stdout reserved for just
the final path). MCP: a JSON-RPC test script drove
`start_recording` -> wait -> `stop_recording` end-to-end - produced a
real 3840x2160, 32-frame recording confirmed valid with
`ffprobe`/`ffmpeg`. Also confirmed the state-management error paths:
`stop_recording` with nothing in progress, and `start_recording`
called twice without an intervening `stop_recording`, both return the
expected tool-level error rather than misbehaving.

## Bugs found via real-session testing

Five real bugs, none catchable by the unit test or code review since
all are specific to actually running PipeWire's mainloop for real -
found across two rounds of live testing on a real desktop session:

1. **Thread-naming panic.** `pipewire-rs` asserts its mainloop is
   created on a thread literally named `"main"`
   (`utils::assert_main_thread`, checked from `Loop::add_signal_local`).
   The recording thread was originally spawned via
   `tokio::task::spawn_blocking`, whose worker threads are named
   `"tokio-rt-worker"` - an immediate panic the moment a real recording
   reached that code path. Fixed by spawning a plain `std::thread`
   explicitly named `"main"` via `std::thread::Builder` instead, and
   `.join()`-ing it directly (blocking is fine - the `current_thread`
   tokio runtime has nothing else to do concurrently during a recording).

2. **Signal race on Ctrl-C.** Without blocking `SIGINT`/`SIGTERM` at the
   OS level before spawning the recording thread, the kernel is free to
   deliver the signal to *either* thread in the process. If it picked the
   real async/main thread (which never touches PipeWire and has no
   custom handler), Rust's default disposition - terminate immediately -
   could win the race against PipeWire's own signalfd-based handling on
   the recording thread, skipping `write_end()` entirely. Observed
   directly: an unpatched Ctrl-C left a 48-byte MP4 (just the `ftyp` +
   empty `mdat` from `write_start()`, confirmed via `xxd`) with no track
   and no video data. Fixed at the time by blocking both signals
   (`libc::pthread_sigmask(SIG_BLOCK, ..)`) on the calling thread
   *before* spawning the recording thread, so the blocked mask is
   inherited and PipeWire's handler is the only possible consumer.
   **Superseded** when MCP `start_recording`/`stop_recording` were
   added (step 4) - `pipewire::channel` replaced this entirely for both
   the CLI and MCP, and the `libc` dependency was dropped.

3. **Framerate negotiation failure.** Added a `VideoFramerate` SPA
   property to the `EnumFormat` pod (min `1/1`, max `30/1`) to address
   bug 4 below, but excluding `0/1` ("variable, damage-driven, no fixed
   rate" - what screen-capture sources commonly report) meant no valid
   intersection existed with what the actual producer offered.
   Negotiation failed outright: `state_changed` went straight to
   `Paused -> Error("no more input formats")`, `process` never got
   called once, size/pixel-format debug logging never even fired.
   Diagnosed by temporarily instrumenting `state_changed`/`param_changed`/
   `process` with `eprintln!` (removed once the root cause was clear).
   Fixed by setting the minimum to `0/1` instead.

4. **CPU-saturated mainloop starves signal handling.** At a demanding
   resolution (3840x2160), `openh264` encoding a frame can take long
   enough that PipeWire keeps handing over new buffers faster than they
   can be drained - the recording thread stays permanently busy inside
   `encode_frame`, with no idle moment left to return to its event loop
   and notice the (already-pending, per bug 2's fix) signalfd. Observed
   directly: `main` thread pinned at ~100% CPU, unresponsive to `kill
   -INT` for 10+ seconds, no `Wrote ...` line ever appearing. Mitigated
   by throttling actual encode work to `MIN_FRAME_INTERVAL` (33ms, ~30
   fps) regardless of how fast buffers arrive - frames arriving faster
   than that are dequeued and dropped immediately (cheap), so the thread
   returns to its event loop often enough for pending signals to be
   serviced promptly even when encoding can't keep up with the source.

5. **Debug builds encode dramatically slower.** Even with bug 4's
   throttle, a `cargo build` (dev profile) recording at 3840x2160 still
   pinned the encoding thread at 100% CPU and stayed unresponsive to
   Ctrl-C for 10+ seconds - because `openh264-sys2`'s `build.rs` uses
   the `cc` crate's default behavior of reading Cargo's `OPT_LEVEL`/
   `DEBUG` env vars, compiling the vendored OpenH264 C++ source with
   *no* optimizations in a dev build. A `cargo build --release` of the
   exact same scenario dropped CPU to ~70% and responded to Ctrl-C
   within ~2 seconds, producing a real 368-frame, 24.7s recording -
   confirmed valid and fully decodable with `ffprobe`/`ffmpeg`.
   **Practical implication: always use `--release` for `prtsc record`
   beyond trivial low-resolution/short use, especially at resolutions
   above 1080p** - not just for speed, but because a debug build can
   make the tool's own Ctrl-C handling sluggish enough to look hung.

### 5. Desktop audio capture - done (`src/recording.rs`)
`record [--audio]` (and MCP `start_recording`'s `audio` parameter) adds
a second, AAC-encoded (`fdk-aac`) track sourced from the *default
sink's monitor* - "what's currently playing," not the microphone. A
direct, unsandboxed PipeWire connection (`ContextRc::new` +
`connect_rc(None)` to the default socket, `STREAM_CAPTURE_SINK =>
"true"`) rather than the portal-scoped one video uses, since desktop
audio isn't portal-gated at all. Runs on the same thread/main loop as
video via its own `StreamRc` (kept alive by its own `CoreRc`/`ContextRc`
internally - no lifetime tying it to a caller-held reference, unlike
video's `StreamBox`), muxed into the same MP4 via lazily assigned,
independently-tracked track ids (`Mp4Writer::add_track` has no getter
to read its own id counter back).

**Verified the stop path doesn't leak the PipeWire node.** Prompted by
a suspicion (raised after real-desktop testing) that recording might be
"leaving the microphone on" - i.e. the capture stream still visibly
active after `stop_recording`/Ctrl-C. Isolated the audio stream's
connect -> quit-the-loop -> drop(stream/core/context) sequence in a
standalone repro binary and watched it via `pw-dump` while keeping the
*owning process* alive afterward (mimicking the long-lived MCP server,
where nothing closes the socket by process exit) - the node
disappeared from the graph within about a second of "stop," well before
the process itself did anything else. So the "quit the main loop, then
drop everything" pattern this codebase already uses for the stop path
(see the start/stop lifecycle section above) does cleanly release the
audio stream; it's not a resource leak in our code.

What *is* real: GNOME Shell's microphone privacy indicator lights up
for **any** active PipeWire audio capture stream, not just ones reading
an actual microphone device - it doesn't distinguish a sink-monitor
("desktop audio") capture from a real mic input, a known, acknowledged
gap in GNOME Shell's own filtering (it should exclude "virtual"
captures based on what they're actually attached to, but doesn't).
`--audio` will visibly trigger that indicator while recording even
though it never touches the mic, and it should still clear promptly on
stop per the `pw-dump` finding above - if it visibly lingers longer
than that, it's the shell's own indicator refresh/polling, not our
stream still being connected.

### 6. Polish - not yet done
- Handle portal cancellation cleanly (same error-surfacing pattern
  `capture` already has) - `screencast::negotiate` does return `Err` on
  failure already, but the "user closed the picker without choosing
  anything" case specifically hasn't been exercised.
- Decide default output naming/location - currently just `recording.mp4`
  in the working directory when no path is given; GNOME's
  `~/Videos/Screencasts/` convention not yet adopted.
- `cargo clippy` / `cargo fmt` - clean as of this pass.

## Why no window list

Earlier revisions of this plan built a `winit` + `softbuffer` + `ratatui`
window with an in-app `xcap`-based window-picker `List`. That was dropped in
two stages:

1. `xcap`'s Linux window enumeration is X11/XCB-only (`_NET_CLIENT_LIST_STACKING`),
   invisible to native Wayland toolkit windows - replaced with `ashpd`'s
   `Screenshot` portal, whose own compositor-drawn picker handles selection
   instead (no caller-side window enumeration needed or possible).
2. Once the portal owned window/screen selection, the custom-rendered window
   had nothing left to justify its existence beyond showing a one-line
   status message - dropped entirely in favor of a plain CLI, with an MCP
   server mode added alongside it for programmatic/agent use.

The `winit`/`softbuffer`/`fontdue`/`ratatui` rendering layer itself still
exists, unused for now, as its own standalone crate at
`../rust-projects/softbuffer-backend` (moved out of this repo's workspace,
full history preserved via `git filter-repo` - see its own history below) -
parked for a possible future interactive/TUI mode, not deleted.

## `softbuffer-backend` crate history (parked, not currently used by `prtsc`, now its own repo)

The steps below built the `softbuffer-backend` crate: a `ratatui` `Backend`
that renders directly into a `winit` window via `softbuffer` and `fontdue`,
with no external terminal emulator involved. The crate still builds and
works standalone; it now lives in its own repo at
`../rust-projects/softbuffer-backend` rather than as a `prtsc` workspace
member, since `prtsc` doesn't depend on it.

### 1. Bare window
Add `winit` and `softbuffer`. Open a window, run the event loop, fill the
softbuffer surface with a solid color on every redraw. No ratatui, no fonts
yet.

**Verify:** window opens, resizes without panicking, closes cleanly on the
close button / Esc.

### 2. Glyph rasterization
Add `fontdue`. Load a single embedded monospace font (e.g. bundle a `.ttf`
under `assets/`, loaded via `include_bytes!`). Rasterize one character at a
fixed size and blit it into the pixel buffer at a fixed position.

**Verify:** one crisp character appears in the window.

### 3. Glyph cache + cell grid
Rasterize the printable ASCII range once at startup into a cache keyed by
`char` (bitmap + advance width/height). Derive terminal-style `(cols, rows)`
from window pixel size and the fixed cell size (monospace, so cell width/
height come straight from the font metrics). Write a function that draws an
arbitrary `&str` grid (`Vec<Vec<char>>` or similar) to the buffer using the
cache.

**Verify:** a small hardcoded grid of text renders correctly, including
non-ASCII fallback behavior (draw blank/`?` for glyphs not in the cache).

**Font coverage note:** DejaVu Sans Mono alone covers every glyph ratatui
uses for borders/gauges/sparklines/scrollbars (box drawing, block elements,
geometric shapes) but has zero Braille Patterns (U+2800–U+28FF) coverage,
needed by ratatui's `Canvas` widget. Checked candidate fonts directly with a
`fontdue`-based coverage scan (see commit history/PR discussion, not kept as
a script in this repo) — DejaVu Sans (the proportional sibling, same
Bitstream Vera license) has full Braille coverage, so `GlyphCache` rasterizes
the printable-ASCII range from DejaVu Sans Mono (also defining the cell
grid) and the Braille range from DejaVu Sans as a fallback, keeping
everything under one already-vetted permissive license. GNU FreeMono also
has full coverage but is GPL-3 with a document-embedding exception that
doesn't clearly cover bundling into a compiled binary — deliberately not
used for that reason.

### 4. `Backend` implementation
Implement `ratatui::backend::Backend` for a `WinitBackend` type wrapping the
window, softbuffer surface, and glyph cache. Required methods: `draw`,
`hide_cursor`, `show_cursor`, `get_cursor_position`, `set_cursor_position`,
`clear`, `size`, `window_size`, `flush`. Map ratatui `Cell` (char + `Style`)
to the glyph cache plus a foreground/background color fill per cell — start
with fg/bg color only, ignore bold/italic/underline for now.

**Verify:** `Terminal::new(WinitBackend::new(...))` constructs successfully
and `terminal.draw(|f| f.render_widget(Paragraph::new("hello"), f.area()))`
shows real text in the window.

### 5. Event loop integration
Wire winit's event loop to drive redraws: request a redraw on window resize/
focus/expose events, and on an application tick (e.g. every 16ms or only
when state changes — start with redraw-on-input for simplicity). Confirm the
window resizes and ratatui re-lays-out content without stale artifacts
(clear-before-draw, resize the softbuffer surface to match).

**Verify:** resizing the window live re-flows a ratatui widget with no
tearing/garbage pixels.

**Known issue (unresolved):** live drag-resizing still shows occasional
artifacts. One real bug in this area was found and fixed — `resize_surface`
was re-querying `window.inner_size()` live instead of using the size the
`WindowEvent::Resized` event itself carried, which raced during fast event
bursts — but the user reports residual artifacts during live drag even after
that fix. Not yet root-caused; revisit before considering step 5 fully done.
Suspects worth checking first: whether `softbuffer`'s platform backend still
has its own lazy/rotating-buffer resize behavior independent of our own
buffer sizing (the same category of bug as the double-buffering flicker
fixed earlier), and whether winit's Wayland `RedrawRequested`-during-resize
gap (rust-windowing/winit#2609) interacts with our tick-driven redraw loop.

## Explicitly out of scope for this plan

- Any VT100/ANSI parsing (`memterm`, `vt100`) — not needed since ratatui
  never emits an escape-sequence stream to this backend.
- Multi-window / tabbed capture UI.
- GIF recording (see "Screen recording" above for video - GIF specifically
  is out of scope).
