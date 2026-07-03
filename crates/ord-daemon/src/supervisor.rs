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

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ord_common::{lock_tolerant, Event};
use ord_core::{CaptureBackend, Engine, FrameStore};

use crate::handler::Handler;
use crate::server::{broadcast, ServerCtx, Subscribers};

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
/// when every sender is dropped.
pub(crate) fn spawn<B, S>(
    handler: Arc<Mutex<Handler<B, S>>>,
    ctx: Arc<Mutex<ServerCtx<B, S>>>,
    subs: Subscribers,
    retry_delay: Duration,
) -> Sender<CaptureRequest<B, S>>
where
    B: CaptureBackend + 'static,
    S: FrameStore + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("ord-capture-supervisor".into())
        .spawn(move || run(rx, handler, ctx, subs, retry_delay))
        .ok();
    tx
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

        match req {
            CaptureRequest::Ensure { reply, retries } => {
                if lock_tolerant(&handler).is_buffer_enabled() {
                    send(reply.as_ref(), Event::BufferState { enabled: true });
                    continue;
                }
                let mut engine = build_engine(&ctx);
                match engine.start() {
                    Ok(()) => {
                        lock_tolerant(&handler).install_engine_preserving_replay(engine);
                        let ev = Event::BufferState { enabled: true };
                        send(reply.as_ref(), ev.clone());
                        broadcast(&subs, &ev);
                        tracing::info!("capture armed");
                    }
                    Err(e) => {
                        let message = format!("failed to start capture: {e}");
                        send(
                            reply.as_ref(),
                            Event::Error {
                                message: message.clone(),
                            },
                        );
                        // A user-dismissed portal picker must not be re-asked
                        // automatically; everything else (portal not ready at
                        // login, transient D-Bus errors) retries on schedule.
                        let cancelled = message.to_ascii_lowercase().contains("cancelled");
                        if retries > 0 && !cancelled {
                            tracing::warn!(
                                error = %message,
                                retries_left = retries - 1,
                                retry_in = ?retry_delay,
                                "capture start failed; will retry"
                            );
                            pending_retry = Some((retries - 1, Instant::now() + retry_delay));
                        } else {
                            tracing::error!(error = %message, "capture start failed; staying degraded (arm again via `ord buffer on` or auto-arm)");
                        }
                    }
                }
            }
            CaptureRequest::Disable { reply } => {
                let ev = lock_tolerant(&handler).disable_capture();
                send(reply.as_ref(), ev.clone());
                if ev.is_state_change() {
                    broadcast(&subs, &ev);
                }
            }
            CaptureRequest::Restart => {
                {
                    let mut h = lock_tolerant(&handler);
                    if !h.is_buffer_enabled() {
                        continue; // disarmed while the request was queued
                    }
                    h.stop_engine_for_restart();
                }
                let mut engine = build_engine(&ctx);
                match engine.start() {
                    Ok(()) => {
                        lock_tolerant(&handler).install_engine_preserving_replay(engine);
                        broadcast(&subs, &Event::CaptureRestarted);
                    }
                    Err(e) => {
                        // The watchdog fires again after its window, which
                        // re-queues a restart — no schedule needed here.
                        broadcast(
                            &subs,
                            &Event::Error {
                                message: format!("capture stalled and restart failed: {e}"),
                            },
                        );
                    }
                }
            }
            CaptureRequest::Swap { mut engine, reply } => {
                if lock_tolerant(&handler).is_buffer_enabled() {
                    if let Err(e) = engine.start() {
                        // The old engine keeps running; the persisted
                        // overrides are retried on the next daemon start.
                        let _ = reply.try_send(Event::Error {
                            message: format!("new capture settings failed to start: {e}"),
                        });
                        continue;
                    }
                }
                // A settings change is a clean cut: buffered footage from the
                // old encoder is dropped with it.
                lock_tolerant(&handler).replace_engine(*engine);
                let _ = reply.try_send(Event::CaptureRestarted);
                broadcast(&subs, &Event::CaptureRestarted);
            }
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
