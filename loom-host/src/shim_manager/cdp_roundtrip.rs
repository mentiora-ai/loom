// ShimManager — the two CDP round-trip primitives every typed sender builds on:
// `cdp_send_one` (a plain request/response with the config recv floor) and
// `cdp_send_dispatch` (a trusted-input round-trip bounded by its budget, where a
// lost ack is reported rather than dead-waited).

use super::helpers::map_shim_code;
use super::process::{send_and_await, send_and_await_dispatch, DispatchAck};
use super::shim_manager::ShimManager;
use super::types::ShimId;
use loom_core::error::{LoomError, LoomErrorCode};
use loom_shared::shim_protocol::{CdpMessage, ShimErrorCode, ShimRequest, ShimResponse};
use std::time::Duration;

impl ShimManager {
    /// One CDP round-trip. `Ok(Ok(payload))` = CDP success; `Ok(Err((code,
    /// detail)))` = the shim reported a CDP-protocol error (an APPLICATION
    /// outcome the caller interprets, e.g. `getBoxModel` on a hidden node);
    /// `Err(LoomError)` = transport failure. Breaker bookkeeping is done by the
    /// caller at its decision points.
    pub(super) async fn cdp_send_one(
        &self,
        id: &ShimId,
        session_id: u64,
        target_id: u64,
        message: CdpMessage,
        budget_ms: u64,
    ) -> Result<Result<ciborium::value::Value, (ShimErrorCode, String)>, LoomError> {
        let config = self.configs.get(id).map(|c| c.clone()).ok_or_else(|| {
            LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {} not registered", id.0),
            )
        })?;
        let process = self.get_or_spawn(id, &config).await?;
        let recv_ms = budget_ms.max(config.recv_timeout_ms);
        match send_and_await(
            &process,
            ShimRequest::CdpSend {
                request_id: 0,
                session_id,
                target_id,
                message,
            },
            Duration::from_millis(config.send_timeout_ms),
            Duration::from_millis(recv_ms),
        )
        .await
        {
            Ok(ShimResponse::Ok { payload, .. }) => Ok(Ok(payload)),
            Ok(ShimResponse::Error { code, detail, .. }) => Ok(Err((code, detail))),
            Ok(other) => Err(LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {}: unexpected CDP response: {other:?}", id.0),
            )),
            Err(e) => Err(e),
        }
    }

    /// One trusted-INPUT CDP round-trip whose ack may be lost to a cross-origin
    /// renderer swap. Unlike [`cdp_send_one`] (whose `budget_ms.max(recv_timeout_ms)`
    /// floor is unchanged, so selector resolution and `web.wait`'s `budget=0`
    /// probes keep their ~30s bound), the recv here is BOUNDED by `budget_ms`, and
    /// a recv timeout is reported as `Ok(None)` (the frame was written, its ack
    /// never came) instead of a full-`recv_timeout` dead-wait. The daemon threads a
    /// real (non-zero) budget to every trusted-input verb; the `budget_ms == 0`
    /// fallback to the config `recv_timeout_ms` floor is a safety for any DIRECT
    /// caller that passes `0` (e.g. an in-crate test), so `0` never means a
    /// zero-length recv. `Ok(Some(Ok(v)))` = CDP success; `Ok(Some(Err(..)))` = CDP
    /// app error; `Ok(None)` = ack timed out within budget; `Err` = the frame never
    /// left the host.
    pub(super) async fn cdp_send_dispatch(
        &self,
        id: &ShimId,
        session_id: u64,
        target_id: u64,
        message: CdpMessage,
        budget_ms: u64,
    ) -> Result<Option<Result<ciborium::value::Value, (ShimErrorCode, String)>>, LoomError> {
        let config = self.configs.get(id).map(|c| c.clone()).ok_or_else(|| {
            LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {} not registered", id.0),
            )
        })?;
        let process = self.get_or_spawn(id, &config).await?;
        // A normal `Input.*` ack returns in tens of ms, so bounding the recv at
        // the caller's budget only ever bites when the ack is genuinely lost (a
        // process swap). `budget_ms == 0` (no caller deadline) falls back to the
        // config floor so verbs that pass `0` are unaffected.
        let recv_ms = if budget_ms > 0 {
            budget_ms
        } else {
            config.recv_timeout_ms
        };
        match send_and_await_dispatch(
            &process,
            ShimRequest::CdpSend {
                request_id: 0,
                session_id,
                target_id,
                message,
            },
            Duration::from_millis(config.send_timeout_ms),
            Duration::from_millis(recv_ms),
        )
        .await?
        {
            DispatchAck::Response(ShimResponse::Ok { payload, .. }) => Ok(Some(Ok(payload))),
            DispatchAck::Response(ShimResponse::Error { code, detail, .. }) => {
                Ok(Some(Err((code, detail))))
            }
            DispatchAck::Response(other) => Err(LoomError::new(
                LoomErrorCode::ShimFailure,
                format!("shim {}: unexpected CDP response: {other:?}", id.0),
            )),
            DispatchAck::RecvTimeout => Ok(None),
        }
    }
}

/// A CDP application error the shim reported (the `Err((code, detail))` half of a
/// `cdp_send_one` payload) as the `LoomError` every sender surfaces:
/// `shim <id>: <detail>`.
pub(super) fn cdp_app_error(id: &ShimId, (code, detail): (ShimErrorCode, String)) -> LoomError {
    LoomError::new(map_shim_code(code), format!("shim {}: {detail}", id.0))
}
