//! The capture supervisor — the only place a capture session ever starts.
//!
//! Starting capture on the real backend can block **indefinitely** on the XDG
//! screen-share portal (a D-Bus call with no timeout). Before the supervisor
//! existed those starts sat in fatal positions: before the control socket was
//! bound (daemon unreachable), under the handler lock (control plane frozen),
//! and on the pump thread (frame draining stalled). The supervisor moves every
//! portal-touching operation onto one dedicated worker thread:
//!
//! * Requests (`Ensure`/`Disable`/`Restart`/`Swap`) arrive over a channel and
//!   are processed strictly one at a time — concurrent arms can never spawn
//!   duplicate portal dialogs.
//! * A start runs with **no locks held**; the handler lock is taken only for
//!   the final engine swap (the pattern `apply_config` proved).
//! * Arm/restart installs preserve the replay state
//!   ([`Handler::install_engine_preserving_replay`]) so recovery never
//!   discards buffered footage or an active recording.
//! * A failed initial arm retries on a bounded schedule (the portal often is
//!   not ready yet at session login), except when the user *cancelled* the
//!   picker — retrying would spam dialogs.
//!
//! If a portal call truly hangs forever, only this worker waits: the control
//! plane (status, saves from the existing buffer, config) stays live, and
//! later capture requests queue behind the hang until the daemon restarts —
//! strictly better than the old "daemon running but unreachable".

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ord_common::{lock_tolerant, Event};
use ord_core::{CaptureBackend, Engine, FrameStore};

use crate::handler::Handler;
use crate::server::{broadcast, ServerCtx, Subscribers};

/// True while the supervisor is processing a capture request (including a
/// blocking portal start). The auto-arm pump consults this so it does not
/// stack Ensure requests / portal dialogs every 3 s.
static BUSY: AtomicBool = AtomicBool::new(false);

/// Whether the supervisor is mid-request (portal start, stop, swap, …).
pub(crate) fn is_busy() -> bool {
    BUSY.load(Ordering::Acquire)
}

/// A capture-lifecycle request. Replies are best-effort: the requester may
/// have stopped waiting (bounded timeout), so sends into `reply` are allowed
/// to fail silently.
pub enum CaptureRequest<B: CaptureBackend, S: FrameStore> {
    /// Arm the replay buffer (start capture) if it isn't already armed.
    /// `retries` failed attempts are re-tried on a schedule (login races).
    Ensure {
        reply: Option<SyncSender<Event>>,
        retries: u32,
    },
    /// Disarm the replay buffer (stop capture, drop buffered footage).
    Disable { reply: Option<SyncSender<Event>> },
    /// Stop the current session and start a fresh one, preserving the replay
    /// state (the watchdog's stall recovery).
    Restart,
    /// Install a caller-built engine (changed encoder settings): started
    /// first if the buffer is armed, then swapped in as a clean cut. Boxed:
    /// an engine is orders of magnitude larger than the other variants.
    Swap {
        engine: Box<Engine<B, S>>,
        reply: SyncSender<Event>,
    },
}

/// Spawn the supervisor worker. Returns the request sender; the worker exits
/// when every sender is dropped. Errors if the OS refuses the thread (rare,
/// but a silent `.ok()` left the daemon degraded with no capture ever).
pub(crate) fn spawn<B, S>(
    handler: Arc<Mutex<Handler<B, S>>>,
    ctx: Arc<Mutex<ServerCtx<B, S>>>,
    subs: Subscribers,
    retry_delay: Duration,
) -> Result<Sender<CaptureRequest<B, S>>, std::io::Error>
where
    B: CaptureBackend + 'static,
    S: FrameStore + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("ord-capture-supervisor".into())
        .spawn(move || run(rx, handler, ctx, subs, retry_delay))?;
    Ok(tx)
}

fn run<B, S>(
    rx: Receiver<CaptureRequest<B, S>>,
    handler: Arc<Mutex<Handler<B, S>>>,
    ctx: Arc<Mutex<ServerCtx<B, S>>>,
    subs: Subscribers,
    retry_delay: Duration,
) where
    B: CaptureBackend + 'static,
    S: FrameStore + 'static,
{
    // A scheduled re-attempt of a failed arm: (attempts left, due time).
    let mut pending_retry: Option<(u32, Instant)> = None;
    loop {
        let req = match pending_retry {
            Some((left, due)) => {
                let now = Instant::now();
                let wait = due.saturating_duration_since(now);
                match rx.recv_timeout(wait) {
                    Ok(r) => {
                        // Any explicit request supersedes the schedule; the
                        // request's own outcome re-arms a retry if warranted.
                        pending_retry = None;
                        r
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        pending_retry = None;
                        CaptureRequest::Ensure {
                            reply: None,
                            retries: left,
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            None => match rx.recv() {
                Ok(r) => r,
                Err(_) => return,
            },
        };

        BUSY.store(true, Ordering::Release);
        process_request(req, &handler, &ctx, &subs, &mut pending_retry, retry_delay);
        BUSY.store(false, Ordering::Release);
    }
}

fn process_request<B, S>(
    req: CaptureRequest<B, S>,
    handler: &Arc<Mutex<Handler<B, S>>>,
    ctx: &Arc<Mutex<ServerCtx<B, S>>>,
    subs: &Subscribers,
    pending_retry: &mut Option<(u32, Instant)>,
    retry_delay: Duration,
) where
    B: CaptureBackend + 'static,
    S: FrameStore + 'static,
{
    match req {
        CaptureRequest::Ensure { reply, retries } => {
            if lock_tolerant(handler).is_buffer_enabled() {
                send(reply.as_ref(), Event::BufferState { enabled: true });
                return;
            }
            let mut engine = build_engine(ctx);
            match engine.start() {
                Ok(()) => {
                    let mut stale = lock_tolerant(handler).install_engine_preserving_replay(engine);
                    // Outgoing engine is empty/stopped; finish any residual
                    // teardown off-lock so a wedged prior session cannot
                    // hold the handler mutex.
                    let _ = stale.stop();
                    // Broadcast BEFORE replying: the requester continues
                    // (and may broadcast follow-ups) the moment the reply
                    // lands, and subscribers must see the state change
                    // first — both go through the same subs-lock FIFO.
                    let ev = Event::BufferState { enabled: true };
                    broadcast(subs, &ev);
                    send(reply.as_ref(), ev);
                    tracing::info!("capture armed");
                }
                Err(e) => {
                    let detail = e.to_string();
                    let message = user_facing_capture_error(&detail);
                    send(
                        reply.as_ref(),
                        Event::Error {
                            message: message.clone(),
                        },
                    );
                    // A user-dismissed portal picker must not be re-asked
                    // automatically; everything else (portal not ready at
                    // login, transient D-Bus errors) retries on schedule.
                    let cancelled = detail.to_ascii_lowercase().contains("cancelled");
                    if retries > 0 && !cancelled {
                        tracing::warn!(
                            error = %detail,
                            user_message = %message,
                            retries_left = retries - 1,
                            retry_in = ?retry_delay,
                            "capture start failed; will retry"
                        );
                        *pending_retry = Some((retries - 1, Instant::now() + retry_delay));
                    } else {
                        tracing::error!(
                            error = %detail,
                            user_message = %message,
                            "capture start failed; staying degraded (arm again via `ord buffer on` or auto-arm)"
                        );
                    }
                }
            }
        }
        CaptureRequest::Disable { reply } => {
            // Detach under the lock, stop off-lock: a hung PipeWire/NVENC
            // teardown must not freeze status/save/config.
            let placeholder = build_engine(ctx);
            let (mut old, ev) = lock_tolerant(handler).disable_capture_detach(placeholder);
            if let Err(e) = old.stop() {
                tracing::debug!(error = %e, "capture stop on disable");
            }
            if ev.is_state_change() {
                broadcast(subs, &ev);
            }
            send(reply.as_ref(), ev);
        }
        CaptureRequest::Restart => {
            // Stop the live session off the handler lock (portal/NVENC
            // teardown can hang). A placeholder keeps the control plane
            // alive and preserves armed intent; the detached buffer is
            // re-adopted into the fresh engine below.
            let placeholder = build_engine(ctx);
            let mut old = {
                let mut h = lock_tolerant(handler);
                if !h.is_buffer_enabled() {
                    return; // disarmed while the request was queued
                }
                h.detach_engine_for_stop(placeholder)
            };
            if let Err(e) = old.stop() {
                tracing::debug!(error = %e, "old engine stop before restart");
            }
            let mut engine = build_engine(ctx);
            match engine.start() {
                Ok(()) => {
                    // Adopt from the detached (stopped) engine so the
                    // ring/markers/recording survive the restart.
                    engine.adopt_replay_from(&mut old);
                    let mut stale = lock_tolerant(handler).exchange_engine(engine, true);
                    let _ = stale.stop();
                    broadcast(subs, &Event::CaptureRestarted);
                }
                Err(e) => {
                    // Put the stopped-but-buffered engine back so the user
                    // can still save; the watchdog will try again (with backoff).
                    let mut stale = lock_tolerant(handler).exchange_engine(old, true);
                    let _ = stale.stop();
                    // Full detail is for operators; the HUD toast must stay short.
                    tracing::error!(error = %e, "capture stalled and restart failed");
                    broadcast(
                        subs,
                        &Event::Error {
                            message: "Capture stalled — restart failed".into(),
                        },
                    );
                }
            }
        }
        CaptureRequest::Swap { mut engine, reply } => {
            if lock_tolerant(handler).is_buffer_enabled() {
                if let Err(e) = engine.start() {
                    // The old engine keeps running; the persisted
                    // overrides are retried on the next daemon start.
                    let detail = e.to_string();
                    tracing::error!(error = %detail, "new capture settings failed to start");
                    let _ = reply.try_send(Event::Error {
                        message: user_facing_capture_error(&detail),
                    });
                    return;
                }
            }
            // A settings change is a clean cut: buffered footage from the
            // old encoder is dropped with it. Any active full-length
            // recording is finalized first (never drop an open muxer).
            // Old capture is stopped off-lock after the exchange.
            let armed = engine.is_running();
            let (rec_events, mut old) = {
                let mut h = lock_tolerant(handler);
                let events = h.cut_active_recording();
                let old = h.exchange_engine(*engine, armed);
                (events, old)
            };
            if let Err(e) = old.stop() {
                tracing::debug!(error = %e, "old engine stop after settings swap");
            }
            for ev in &rec_events {
                broadcast(subs, ev);
            }
            broadcast(subs, &Event::CaptureRestarted);
            let _ = reply.try_send(Event::CaptureRestarted);
        }
    }
}

fn build_engine<B, S>(ctx: &Arc<Mutex<ServerCtx<B, S>>>) -> Engine<B, S>
where
    B: CaptureBackend,
    S: FrameStore,
{
    // Construction is cheap and portal-free; only `start()` may block, and it
    // runs after this lock is released.
    let c = lock_tolerant(ctx);
    let cfg = lock_tolerant(&c.config).clone();
    (c.engine_factory)(&cfg)
}

fn send(reply: Option<&SyncSender<Event>>, ev: Event) {
    if let Some(tx) = reply {
        let _ = tx.try_send(ev);
    }
}

/// Short HUD/CLI-friendly capture error. Full `detail` belongs in logs only.
fn user_facing_capture_error(detail: &str) -> String {
    let d = detail.to_ascii_lowercase();
    if d.contains("cancelled") || d.contains("canceled") {
        return "Screen share cancelled — run ord buffer on".into();
    }
    if d.contains("permission") || d.contains("denied") || d.contains("not authorized") {
        return "Screen share denied — re-approve portal share".into();
    }
    if d.contains("nvenc") || d.contains("cuda") || d.contains("encoder") {
        return "Encoder init failed — check NVIDIA / ord doctor".into();
    }
    if d.contains("stall") || d.contains("restart") {
        return "Capture stalled — restart failed".into();
    }
    if d.contains("initialization") || d.contains("init") {
        return "Capture init failed — try buffer off/on".into();
    }
    // Generic short form; never paste the full backend dump into the toast.
    "Capture failed to start".into()
}

#[cfg(test)]
mod user_facing_tests {
    use super::user_facing_capture_error;

    #[test]
    fn maps_common_failures_to_short_lines() {
        assert!(user_facing_capture_error("portal: cancelled by user").contains("cancelled"));
        assert!(user_facing_capture_error("NVENC open failed").contains("Encoder"));
        assert!(
            user_facing_capture_error("capture initialization failed: x").contains("init failed")
        );
        assert!(
            user_facing_capture_error("capture stalled and restart failed: y").contains("stalled")
        );
        // Never echo a multi-sentence dump.
        let long = "a".repeat(200);
        assert!(user_facing_capture_error(&long).len() < 80);
    }
}
