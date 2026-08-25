//! An MCP server, served over stdio, exposing the screenshot-portal capture
//! action and screencast recording as tools.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt, schemars, serde, tool, tool_handler,
    tool_router,
};

use crate::capture;
use crate::recording::{self, Terminate};
use crate::screencast;

/// A recording started by `start_recording`, kept until `stop_recording`
/// finishes it.
struct RecordingHandle {
    sender: pipewire::channel::Sender<Terminate>,
    task: tokio::task::JoinHandle<Result<(), String>>,
    path: PathBuf,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct StartRecordingRequest {
    /// Output file path. Defaults to `recording.mp4` in the working
    /// directory if omitted.
    path: Option<String>,
    /// Also capture desktop audio (the default sink's monitor) as a second
    /// track. Defaults to false (video only) if omitted.
    audio: Option<bool>,
}

#[derive(Clone)]
struct CaptureServer {
    tool_router: ToolRouter<Self>,
    /// At most one recording in flight at a time - `None` when idle.
    recording: Arc<Mutex<Option<RecordingHandle>>>,
}

#[tool_router]
impl CaptureServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            recording: Arc::new(Mutex::new(None)),
        }
    }

    #[tool(
        description = "Initiate a screen capture via the system's screenshot picker and return the saved file's location"
    )]
    async fn capture(&self) -> Result<CallToolResult, McpError> {
        match capture::capture().await {
            Ok(uri) => Ok(CallToolResult::success(vec![Content::text(uri)])),
            Err(err) => Ok(CallToolResult::error(vec![Content::text(err)])),
        }
    }

    #[tool(
        description = "Start recording a screencast via the system's screen-share picker, \
                        optionally with desktop audio (the default sink's monitor) as a \
                        second track. Blocks until the user completes that picker and \
                        recording has actually begun. Call stop_recording to finish and get \
                        the saved file's location - only one recording can be in progress \
                        at a time."
    )]
    async fn start_recording(
        &self,
        Parameters(StartRecordingRequest { path, audio }): Parameters<StartRecordingRequest>,
    ) -> Result<CallToolResult, McpError> {
        let audio = audio.unwrap_or(false);
        if self.recording.lock().unwrap().is_some() {
            return Ok(CallToolResult::error(vec![Content::text(
                "a recording is already in progress - call stop_recording first",
            )]));
        }

        let session = match screencast::negotiate().await {
            Ok(session) => session,
            Err(err) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "recording setup failed: {err}"
                ))]));
            }
        };

        let path = PathBuf::from(path.unwrap_or_else(|| "recording.mp4".to_string()));
        let thread_path = path.clone();
        let (sender, stop_rx) = pipewire::channel::channel::<Terminate>();
        // See prtsc::record's equivalent thread spawn for why this needs to
        // be a plain, "main"-named std::thread rather than spawn_blocking.
        let handle = match std::thread::Builder::new()
            .name("main".to_string())
            .spawn(move || {
                let session = session;
                recording::record(
                    session.fd,
                    session.node_id,
                    session.size,
                    &thread_path,
                    audio,
                    stop_rx,
                )
            }) {
            Ok(handle) => handle,
            Err(err) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "failed to spawn recording thread: {err}"
                ))]));
            }
        };
        let task =
            tokio::task::spawn_blocking(move || handle.join().expect("recording thread panicked"));

        let message = format!("Recording started: {}", path.display());
        *self.recording.lock().unwrap() = Some(RecordingHandle { sender, task, path });
        Ok(CallToolResult::success(vec![Content::text(message)]))
    }

    #[tool(
        description = "Stop the in-progress recording started by start_recording and return \
                        the saved file's location."
    )]
    async fn stop_recording(&self) -> Result<CallToolResult, McpError> {
        let Some(handle) = self.recording.lock().unwrap().take() else {
            return Ok(CallToolResult::error(vec![Content::text(
                "no recording in progress",
            )]));
        };
        let _ = handle.sender.send(Terminate);
        match handle.task.await.expect("recording thread panicked") {
            Ok(()) => Ok(CallToolResult::success(vec![Content::text(
                handle.path.display().to_string(),
            )])),
            Err(err) => Ok(CallToolResult::error(vec![Content::text(err)])),
        }
    }
}

#[tool_handler]
impl ServerHandler for CaptureServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Exposes tools to initiate a screen capture via the system's screenshot \
                 portal, and to start/stop recording a screencast via the system's \
                 screen-share portal."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

/// Runs the MCP server over stdio until the client disconnects.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let service = CaptureServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
