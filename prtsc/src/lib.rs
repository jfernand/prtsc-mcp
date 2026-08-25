//! A screen capture/recording CLI tool built on the XDG Desktop Portal:
//! `prtsc` with no arguments captures a screenshot once and prints the
//! saved location, `prtsc record [--audio] [path]` records a screencast
//! (optionally with desktop audio) until Ctrl-C, and `prtsc mcp` exposes
//! the capture/recording actions as MCP tools over stdio.
#![warn(missing_docs)]

mod capture;
mod mcp;
mod recording;
mod screencast;

/// Runs `prtsc` according to the first CLI argument: a one-shot capture
/// with no arguments, `record [path]` to record a screencast until
/// Ctrl-C, or the MCP server for `mcp`.
///
/// # Examples
///
/// ```no_run
/// // Reads real CLI args and may block on a capture or an MCP session, so
/// // this is `no_run` rather than an executed doctest.
/// prtsc::run();
/// ```
pub fn run() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    runtime.block_on(run_async());
}

async fn run_async() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => capture_once().await,
        Some("mcp") => {
            if let Err(err) = mcp::run().await {
                eprintln!("mcp server error: {err}");
                std::process::exit(1);
            }
        }
        Some("record") => {
            let mut audio = false;
            let mut path = None;
            for arg in args {
                if arg == "--audio" {
                    audio = true;
                } else if path.is_none() {
                    path = Some(arg);
                } else {
                    eprintln!("unexpected argument: {arg}");
                    std::process::exit(2);
                }
            }
            record(path, audio).await
        }
        Some(other) => {
            eprintln!(
                "unknown argument: {other} (expected no arguments, `mcp`, or `record [--audio] [path]`)"
            );
            std::process::exit(2);
        }
    }
}

async fn capture_once() {
    match capture::capture().await {
        Ok(uri) => println!("{uri}"),
        Err(err) => {
            eprintln!("capture failed: {err}");
            std::process::exit(1);
        }
    }
}

async fn record(output: Option<String>, audio: bool) {
    let output = output.unwrap_or_else(|| "recording.mp4".to_string());

    let session = match screencast::negotiate().await {
        Ok(session) => session,
        Err(err) => {
            eprintln!("recording setup failed: {err}");
            std::process::exit(1);
        }
    };

    let path = std::path::PathBuf::from(output);
    let (sender, stop_rx) = pipewire::channel::channel::<recording::Terminate>();
    let thread_path = path.clone();
    // `pipewire-rs` asserts its mainloop is created on a thread literally
    // named "main" (see `utils::assert_main_thread`), which a
    // `tokio::task::spawn_blocking` worker (named "tokio-rt-worker") isn't -
    // discovered the hard way when this panicked on a real recording
    // attempt. A plain `std::thread` explicitly named "main" satisfies it.
    let handle = std::thread::Builder::new()
        .name("main".to_string())
        .spawn(move || {
            // `session` (specifically its ashpd `Session`) must stay alive
            // for the whole recording - dropping it ends the portal-side
            // cast - so it's moved into this closure whole rather than
            // just its fields.
            let session = session;
            recording::record(
                session.fd,
                session.node_id,
                session.size,
                &thread_path,
                audio,
                stop_rx,
            )
        })
        .expect("failed to spawn recording thread");

    // `.join()` is blocking, so it's wrapped in `spawn_blocking` to make it
    // awaitable - letting it race against Ctrl-C/SIGTERM below rather than
    // relying on OS signal delivery, which real testing found to be
    // genuinely unreliable across threads (see the implementation plan):
    // whichever thread the kernel happened to deliver the signal to could
    // race with and beat pipewire's own handling, skipping the clean
    // shutdown/`write_end` path entirely.
    let join_task =
        tokio::task::spawn_blocking(move || handle.join().expect("recording thread panicked"));
    tokio::pin!(join_task);

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to install SIGTERM handler");
    let result = tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            let _ = sender.send(recording::Terminate);
            (&mut join_task).await.expect("join task panicked")
        }
        _ = sigterm.recv() => {
            let _ = sender.send(recording::Terminate);
            (&mut join_task).await.expect("join task panicked")
        }
        result = &mut join_task => result.expect("join task panicked"),
    };

    match result {
        Ok(()) => println!("{}", path.display()),
        Err(err) => {
            eprintln!("recording failed: {err}");
            std::process::exit(1);
        }
    }
}
