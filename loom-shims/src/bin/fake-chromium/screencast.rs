//! Synthetic screencast frames (video-capture e2e).

use futures::SinkExt;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

use crate::conn::{stamp, Closed, Conn};

/// A minimal valid 1×1 JPEG, base64-encoded — emitted as a synthetic screencast
/// frame (video-capture e2e). Valid JPEG bytes so a real-ffmpeg encode in the
/// `#[ignore]`d e2e succeeds; the shim recorder only base64-decodes + buffers it.
pub(crate) const TINY_JPEG_BASE64: &str = "/9j/4AAQSkZJRgABAgAAAQABAAD//gAQTGF2YzYyLjI4LjEwMQD/2wBDAAgEBAQEBAUFBQUFBQYGBgYGBgYGBgYGBgYHBwcICAgHBwcGBgcHCAgICAkJCQgICAgJCQoKCgwMCwsODg4RERT/xABLAAEBAAAAAAAAAAAAAAAAAAAACAEBAAAAAAAAAAAAAAAAAAAAABABAAAAAAAAAAAAAAAAAAAAABEBAAAAAAAAAAAAAAAAAAAAAP/AABEIAAIAAgMBIgACEQADEQD/2gAMAwEAAhEDEQA/AJ/AB//Z";

impl Conn {
    /// video-capture: on Page.startScreencast, emit N synthetic
    /// Page.screencastFrame events (each a tiny valid JPEG) so the shim's
    /// ScreencastRecorder can be exercised end-to-end without real Chromium.
    /// N comes from LOOM_FAKE_CHROMIUM_SCREENCAST_FRAMES (default 0 = none).
    /// The shim acks each frame via Page.screencastFrameAck (default {} Ok).
    pub(crate) async fn emit_screencast_frames(
        &mut self,
        session_id: &Option<String>,
    ) -> Result<(), Closed> {
        let n: u32 = std::env::var("LOOM_FAKE_CHROMIUM_SCREENCAST_FRAMES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        for frame_no in 1..=n {
            let mut frame = json!({
                "method": "Page.screencastFrame",
                "params": {
                    "data": TINY_JPEG_BASE64,
                    "metadata": {
                        "offsetTop": 0,
                        "pageScaleFactor": 1,
                        "deviceWidth": 1,
                        "deviceHeight": 1,
                        "scrollOffsetX": 0,
                        "scrollOffsetY": 0,
                        "timestamp": frame_no as f64,
                    },
                    "sessionId": frame_no,
                },
            });
            stamp(&mut frame, session_id);
            if self
                .write
                .send(Message::Text(frame.to_string().into()))
                .await
                .is_err()
            {
                return Err(Closed);
            }
        }
        Ok(())
    }
}
