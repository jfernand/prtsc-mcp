//! Triggers the XDG Desktop Portal's screenshot picker.

use ashpd::desktop::screenshot::Screenshot;

/// The saved screenshot's location on success, or a human-readable error.
pub type CaptureResult = Result<String, String>;

/// Requests a screenshot via the XDG Desktop Portal's interactive picker and
/// returns the saved file's location once the user completes it.
pub async fn capture() -> CaptureResult {
    let request = Screenshot::request()
        .interactive(true)
        .modal(true)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    let response = request.response().map_err(|err| err.to_string())?;
    Ok(response.uri().to_string())
}
