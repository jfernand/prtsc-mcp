//! Negotiates a screencast session with the XDG Desktop Portal's
//! `ScreenCast` interface, handing back a PipeWire-ready file descriptor.
//!
//! Unlike [`crate::capture`], this doesn't return a saved file - it hands
//! the caller a live PipeWire node to consume themselves (see
//! [`crate::recording`]).

use std::os::fd::OwnedFd;

use ashpd::desktop::Session;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};

/// A negotiated screencast session: a PipeWire remote scoped to exactly the
/// stream the user picked in the portal's dialog. Keeping `session` alive
/// for the recording's duration is required - dropping it ends the cast.
pub struct ScreencastSession {
    _session: Session<Screencast>,
    pub fd: OwnedFd,
    pub node_id: u32,
    pub size: (i32, i32),
}

/// Requests a screencast via the XDG Desktop Portal's interactive picker and
/// returns a PipeWire remote for the stream the user selected.
pub async fn negotiate() -> Result<ScreencastSession, String> {
    let proxy = Screencast::new().await.map_err(|err| err.to_string())?;
    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(|err| err.to_string())?;

    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(SourceType::Monitor | SourceType::Window)
                .set_multiple(false),
        )
        .await
        .map_err(|err| err.to_string())?;

    let streams = proxy
        .start(&session, None, Default::default())
        .await
        .map_err(|err| err.to_string())?
        .response()
        .map_err(|err| err.to_string())?;
    let stream = streams
        .streams()
        .first()
        .ok_or("portal returned no streams")?;
    let node_id = stream.pipe_wire_node_id();
    let size = stream.size().ok_or("portal stream has no size")?;

    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(|err| err.to_string())?;

    Ok(ScreencastSession {
        _session: session,
        fd,
        node_id,
        size,
    })
}
