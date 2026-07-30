//! Holding the session-lock confirmation back until a lock frame has actually
//! reached the screen.
//!
//! `ext-session-lock-v1` forbids the `locked` event until a locked frame has
//! been presented on every output, so entering [`SessionLock::Locked`] and
//! telling the client the session is locked are two separate steps: the lock
//! surface's first commit does the former, and the outputs reporting presented
//! lock frames does the latter.

use std::collections::HashSet;
use std::time::Duration;

use driftwm::protocols::output_power::OutputPowerHandler;
use smithay::{
    output::Output,
    reexports::{
        calloop::{
            RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        drm::control::crtc,
        wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1,
    },
    wayland::session_lock::SessionLocker,
};

use super::{DriftWm, SessionLock};

/// How long to wait for the outstanding outputs before blanking them instead.
///
/// A backstop has to exist because never confirming is the worst outcome
/// available: a before-sleep locker holds a systemd sleep-delay inhibitor until
/// `locked` arrives, and systemd suspends anyway once `InhibitDelayMaxSec` runs
/// out — i.e. suspends with an unconfirmed lock. The failure budget is
/// blank-then-confirm, never never-confirm.
pub const LOCK_CONFIRM_TIMEOUT: Duration = Duration::from_secs(1);

impl DriftWm {
    /// Enter [`SessionLock::Locked`] on the lock surface's first commit and
    /// start waiting for the outputs to put a lock frame on screen.
    ///
    /// The gap between entering the state and sending `locked` is one refresh on
    /// healthy paths, but smithay only flips its own `lock_status` when the
    /// locker is consumed — so inside that gap a plain `destroy` is accepted
    /// without `invalid_destroy` (leaving a locked session with a dead lock
    /// object, which the dead-client replacement path recovers), and an
    /// `unlock_and_destroy` posts `invalid_unlock` and then unlocks regardless
    /// (smithay's request handler has no early return after the error), so the
    /// client is killed by a fatal protocol error *and* the session comes
    /// unlocked where it previously just worked. That `unlock()` can also drop a
    /// `pending_confirmation` still holding the locker, whose `Drop` sends
    /// `finished` — an event this path never produced before the confirmation
    /// was deferred.
    pub fn enter_locked(&mut self, locker: SessionLocker) {
        let lock = locker.ext_session_lock().clone();
        #[allow(clippy::mutable_key_type)]
        let awaiting_present: HashSet<Output> = self
            .active_outputs
            .iter()
            .filter(|o| !self.confirmed_dark(o))
            .cloned()
            .collect();
        // Outputs other than the one that just committed may already be sitting
        // on their blank `Pending` frame with no redraw pending, so nothing
        // would produce the frame we are about to wait for.
        self.mark_all_dirty();
        self.cancel_lock_confirm_timer();

        if awaiting_present.is_empty() {
            // Also the winit and fixture case: `active_outputs` is populated by
            // the udev backend alone, so there is never anything to wait for
            // (and `mark_all_dirty` above is likewise a no-op). Deliberate — the
            // nested backend's host compositor owns the screen.
            locker.lock();
            self.session_lock = SessionLock::Locked {
                lock,
                pending_confirmation: None,
                awaiting_present,
            };
            tracing::info!("Session lock confirmed: no outputs to wait on");
            return;
        }

        self.session_lock = SessionLock::Locked {
            lock: lock.clone(),
            pending_confirmation: Some(locker),
            awaiting_present,
        };
        self.arm_lock_confirm_timer(lock);
    }

    /// An output whose panel is genuinely dark. `dpms_off_outputs` alone is not
    /// enough: it is written when the client asks, while the `compositor.clear()`
    /// that darkens the panel happens later in the render loop's drain — in
    /// between, the output is still lit and still showing the desktop.
    fn confirmed_dark(&self, output: &Output) -> bool {
        self.dpms_off_outputs.contains(output) && !self.pending_dpms.contains_key(output)
    }

    /// Whether `output` still owes a presented lock frame.
    pub fn is_awaiting_lock_frame(&self, output: &Output) -> bool {
        matches!(
            &self.session_lock,
            SessionLock::Locked { awaiting_present, .. } if awaiting_present.contains(output)
        )
    }

    /// An output no longer owes a lock frame — it presented one, went dark, or
    /// went away. Only the first of those is a present, hence the name: the
    /// caller has established that nothing unlocked can be visible there. Sends
    /// `locked` once the last awaited output reports in.
    ///
    /// Called from inside the DRM device borrow, so it must not touch
    /// `udev_device`.
    pub fn stop_awaiting_lock_frame(&mut self, output: &Output) {
        let SessionLock::Locked {
            pending_confirmation,
            awaiting_present,
            ..
        } = &mut self.session_lock
        else {
            return;
        };
        if !awaiting_present.remove(output) || !awaiting_present.is_empty() {
            return;
        }
        let locker = pending_confirmation.take();
        self.finish_lock_confirmation(locker, "every awaited output reported in");
    }

    /// Another VT owns the CRTCs, so none of our frames are on any panel and no
    /// output can show unlocked content. Sound with no deadline at all, and it
    /// spares a VT switch the full timeout.
    pub fn confirm_lock_on_session_pause(&mut self) {
        let SessionLock::Locked {
            pending_confirmation,
            awaiting_present,
            ..
        } = &mut self.session_lock
        else {
            return;
        };
        if awaiting_present.is_empty() {
            return;
        }
        awaiting_present.clear();
        let locker = pending_confirmation.take();
        self.finish_lock_confirmation(locker, "another session took the CRTCs");
    }

    /// A CRTC's frame provenance no longer describes anything on a panel — it
    /// went dark, went away, or belongs to a session that no longer owns it.
    pub fn forget_lock_frame(&mut self, crtc: crtc::Handle) {
        self.lock_frame_queued.remove(&crtc);
        self.lock_frame_on_screen.remove(&crtc);
    }

    /// As [`Self::forget_lock_frame`], for every CRTC at once.
    pub fn clear_lock_frames(&mut self) {
        self.lock_frame_queued.clear();
        self.lock_frame_on_screen.clear();
    }

    fn finish_lock_confirmation(&mut self, locker: Option<SessionLocker>, reason: &str) {
        self.cancel_lock_confirm_timer();
        if let Some(locker) = locker {
            locker.lock();
            tracing::info!("Session lock confirmed: {reason}");
        }
    }

    pub fn cancel_lock_confirm_timer(&mut self) {
        if let Some(token) = self.lock_confirm_timer.take() {
            self.loop_handle.remove(token);
        }
    }

    /// Cancel the pending deadline timer, if any.
    pub fn cancel_pending_deadline(&mut self) {
        if let SessionLock::Pending { deadline_token, .. } = &mut self.session_lock
            && let Some(token) = deadline_token.take()
        {
            self.loop_handle.remove(token);
        }
    }

    /// Arm a one-second deadline that forces `Pending → Locked` even if not all
    /// lock surfaces have committed. Returns `None` when the timer cannot be
    /// inserted (the compositor will keep showing the desktop until the surfaces
    /// commit, which may never come without the backstop).
    pub fn arm_pending_deadline(&mut self) -> Option<RegistrationToken> {
        match self.loop_handle.insert_source(
            Timer::from_duration(Duration::from_millis(1000)),
            |_, _, data: &mut DriftWm| {
                if !matches!(data.session_lock, SessionLock::Pending { .. }) {
                    return TimeoutAction::Drop;
                }
                let old = std::mem::replace(&mut data.session_lock, SessionLock::Unlocked);
                if let SessionLock::Pending { locker, .. } = old {
                    data.enter_locked(locker);
                }
                TimeoutAction::Drop
            },
        ) {
            Ok(token) => {
                tracing::trace!("Pending lock deadline armed (1s)");
                Some(token)
            }
            Err(e) => {
                tracing::error!("Failed to arm pending lock deadline: {e:?}");
                None
            }
        }
    }

    fn arm_lock_confirm_timer(&mut self, lock: ExtSessionLockV1) {
        // Repeating, not one-shot: blanking an output only *requests* DPMS off,
        // and any input undoes that request before the drain runs
        // (`wake_dpms_off_outputs` fires ahead of the locked-input gate), which
        // re-lights the output instead of clearing it. A retired timer would
        // leave that output awaited forever. The token is dropped by
        // `finish_lock_confirmation` and the cancel paths, whose identity check
        // is what keeps re-arming from ever confirming a later client's lock.
        let timer = self.loop_handle.insert_source(
            Timer::from_duration(LOCK_CONFIRM_TIMEOUT),
            move |_, _, data: &mut DriftWm| {
                data.blank_outputs_owing_lock_frames(&lock);
                TimeoutAction::ToDuration(LOCK_CONFIRM_TIMEOUT)
            },
        );
        match timer {
            Ok(token) => self.lock_confirm_timer = Some(token),
            Err(e) => tracing::error!(
                "Session lock: failed to arm the confirmation backstop ({e:?}) — an output that \
                 never presents a lock frame will now leave the lock unconfirmed indefinitely"
            ),
        }
    }

    /// Backstop: the outstanding outputs never presented a lock frame, so darken
    /// them rather than claim a lock that is not on screen. The DPMS-off request
    /// routes through the render loop's drain, whose `compositor.clear()` really
    /// does blank the panel and reports in like a present would — meeting the
    /// protocol's requirement instead of skipping it. The user's next input
    /// wakes the outputs again.
    fn blank_outputs_owing_lock_frames(&mut self, lock: &ExtSessionLockV1) {
        // A token that outlived its lock must not confirm a later client's: the
        // first client can die and be replaced between arming and firing.
        let SessionLock::Locked {
            lock: live,
            awaiting_present,
            ..
        } = &self.session_lock
        else {
            return;
        };
        if live != lock {
            return;
        }

        for output in awaiting_present.iter().cloned().collect::<Vec<_>>() {
            let crtc = self
                .udev_device
                .as_ref()
                .and_then(|dev| dev.crtc_for_output(&output));
            tracing::warn!(
                "Session lock: '{}' presented no lock frame within {LOCK_CONFIRM_TIMEOUT:?}, \
                 blanking it (redraw_pending={}, frame_in_flight={}, vblank_timer={}, dpms_off={})",
                output.name(),
                self.redraws_needed.contains(&output),
                crtc.is_some_and(|c| self.frames_pending.contains(&c)),
                crtc.is_some_and(|c| self.estimated_vblank_timers.contains_key(&c)),
                self.dpms_off_outputs.contains(&output),
            );
            // Already off means the drain that clears it simply hasn't run yet
            // (a genuinely dark output was never awaited); asking again would
            // cancel that pending transition instead of hastening it.
            if !self.dpms_off_outputs.contains(&output) {
                // `set_dpms` silently does nothing without a seat session, which
                // would leave the last line of defence inert.
                if self.session.is_none() {
                    tracing::error!(
                        "Session lock: cannot blank '{}' with no seat session — the confirmation \
                         backstop is inert and this lock cannot be confirmed",
                        output.name()
                    );
                } else {
                    OutputPowerHandler::set_dpms(self, &output, false);
                }
            }
        }
    }
}
