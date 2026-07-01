use evdev::{AbsoluteAxisType, InputEventKind, Key};
use std::io;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time;

use crate::{EngineStatus, MomentumMessage, ResolvedArgs, decision_log::DecisionLog};

const RING_SIZE: usize = 100;
const VELOCITY_WINDOW_SAMPLES: usize = 8;
const LIFT_TAIL_SAMPLES: usize = 10;
const LIFT_TAIL_SPEED_RATIO: f64 = 0.45;
const MIN_TOUCH_US: u64 = 130_000;
const MIN_MOTION_DISTANCE: f64 = 40.0;
const RETOUCH_ARM_DELAY_US: u64 = 200_000;
#[derive(Debug, Clone, Copy, PartialEq)]
enum ListenerState {
    Idle,
    OneFingerMove,
    MomentumRetouch,
}

#[derive(Debug, Clone, Copy)]
enum GrabAction {
    Grab(&'static str),
    Ungrab(&'static str),
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    x: i32,
    y: i32,
    ts_us: u64,
}

struct MotionRing {
    buf: [Sample; RING_SIZE],
    pos: usize,
    count: usize,
    total_count: u64,
    total_distance: f64,
}

#[derive(Debug, Clone, Copy)]
struct MotionSummary {
    samples: usize,
    dx: i32,
    dy: i32,
    distance: f64,
    path_distance: f64,
    duration_us: u64,
}

impl MotionSummary {
    fn path_speed(&self) -> f64 {
        if self.duration_us == 0 {
            return 0.0;
        }
        self.path_distance / (self.duration_us as f64 / 1_000_000.0)
    }
}

#[derive(Debug, Clone, Copy)]
struct LiftTailTrim {
    tail_samples: usize,
    tail_speed: f64,
    previous_speed: f64,
    trim_threshold: f64,
}

#[derive(Debug, Clone, Copy)]
struct ReleaseVelocity {
    vx: f64,
    vy: f64,
    summary: MotionSummary,
    age_us: u64,
    trim: Option<LiftTailTrim>,
}

impl MotionRing {
    fn new() -> Self {
        Self {
            buf: [Sample {
                x: 0,
                y: 0,
                ts_us: 0,
            }; RING_SIZE],
            pos: 0,
            count: 0,
            total_count: 0,
            total_distance: 0.0,
        }
    }

    fn clear(&mut self) {
        self.pos = 0;
        self.count = 0;
        self.total_count = 0;
        self.total_distance = 0.0;
    }

    fn push(&mut self, x: i32, y: i32, ts_us: u64) {
        if self.count > 0 {
            let prev = self.nth_oldest(self.count - 1);
            let dx = x - prev.x;
            let dy = y - prev.y;
            self.total_distance += ((dx * dx + dy * dy) as f64).sqrt();
        }

        self.buf[self.pos] = Sample { x, y, ts_us };
        self.pos = (self.pos + 1) % RING_SIZE;
        self.total_count += 1;
        if self.count < RING_SIZE {
            self.count += 1;
        }
    }

    fn count(&self) -> usize {
        self.count
    }

    fn total_count(&self) -> u64 {
        self.total_count
    }

    fn total_distance(&self) -> f64 {
        self.total_distance
    }

    fn motion_summary(&self) -> Option<MotionSummary> {
        self.recent_summary(VELOCITY_WINDOW_SAMPLES)
    }

    fn recent_summary(&self, samples: usize) -> Option<MotionSummary> {
        let samples = samples.min(self.count);
        if samples < 2 {
            return None;
        }
        self.summary_between(self.count - samples, self.count - 1)
    }

    fn summary_between(&self, start: usize, end: usize) -> Option<MotionSummary> {
        if start >= self.count || end >= self.count || start >= end {
            return None;
        }

        let oldest = self.nth_oldest(start);
        let newest = self.nth_oldest(end);
        let dx = newest.x - oldest.x;
        let dy = newest.y - oldest.y;
        let distance = ((dx * dx + dy * dy) as f64).sqrt();
        let duration_us = newest.ts_us.saturating_sub(oldest.ts_us);

        let mut path_distance = 0.0;
        for idx in (start + 1)..=end {
            let prev = self.nth_oldest(idx - 1);
            let current = self.nth_oldest(idx);
            let step_dx = current.x - prev.x;
            let step_dy = current.y - prev.y;
            path_distance += ((step_dx * step_dx + step_dy * step_dy) as f64).sqrt();
        }

        Some(MotionSummary {
            samples: end - start + 1,
            dx,
            dy,
            distance,
            path_distance,
            duration_us,
        })
    }

    fn compute_release_velocity(
        &self,
        now_us: u64,
        stale_us: u64,
        min_velocity: f64,
    ) -> Option<ReleaseVelocity> {
        if self.count < 2 {
            return None;
        }

        let newest = self.nth_oldest(self.count - 1);
        if now_us.saturating_sub(newest.ts_us) > stale_us {
            return None;
        }

        let mut end = self.count - 1;
        let mut trim = None;

        if self.count >= VELOCITY_WINDOW_SAMPLES + LIFT_TAIL_SAMPLES {
            let tail_start = self.count - LIFT_TAIL_SAMPLES;
            let tail_end = self.count - 1;
            let previous_end = tail_start - 1;
            let previous_start = previous_end + 1 - VELOCITY_WINDOW_SAMPLES;

            if let (Some(tail), Some(previous)) = (
                self.summary_between(tail_start, tail_end),
                self.summary_between(previous_start, previous_end),
            ) {
                let tail_speed = tail.path_speed();
                let previous_speed = previous.path_speed();
                let trim_threshold =
                    (previous_speed * LIFT_TAIL_SPEED_RATIO).max(min_velocity * 2.0);

                if previous_speed >= min_velocity && tail_speed < trim_threshold {
                    end = previous_end;
                    trim = Some(LiftTailTrim {
                        tail_samples: LIFT_TAIL_SAMPLES,
                        tail_speed,
                        previous_speed,
                        trim_threshold,
                    });
                }
            }
        }

        let end_sample = self.nth_oldest(end);
        let age_us = now_us.saturating_sub(end_sample.ts_us);
        if age_us > stale_us {
            return None;
        }

        let samples = VELOCITY_WINDOW_SAMPLES.min(end + 1);
        let start = end + 1 - samples;
        let summary = self.summary_between(start, end)?;
        if summary.duration_us == 0 {
            return None;
        }

        let dt = summary.duration_us as f64 / 1_000_000.0;
        Some(ReleaseVelocity {
            vx: summary.dx as f64 / dt,
            vy: summary.dy as f64 / dt,
            summary,
            age_us,
            trim,
        })
    }

    fn nth_oldest(&self, n: usize) -> Sample {
        let start = if self.pos >= self.count {
            self.pos - self.count
        } else {
            RING_SIZE - (self.count - self.pos)
        };
        self.buf[(start + n) % RING_SIZE]
    }
}

fn timestamp_to_us(ts: time::SystemTime) -> u64 {
    ts.duration_since(time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

fn set_nonblocking(device: &evdev::Device) -> io::Result<()> {
    let fd = device.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }

    let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

fn grab_touchpad(
    device: &mut evdev::Device,
    grabbed: &mut bool,
    decision_log: &DecisionLog,
    reason: &str,
) {
    if *grabbed {
        return;
    }

    match device.grab() {
        Ok(()) => {
            *grabbed = true;
            decision_log.line(format!("TOUCHPAD_GRAB reason={}", reason));
            log::debug!("Touchpad grabbed: {}", reason);
        }
        Err(e) => {
            decision_log.line(format!(
                "TOUCHPAD_GRAB_FAILED reason={} error={}",
                reason, e
            ));
            log::warn!("Could not grab touchpad: {}", e);
        }
    }
}

fn ungrab_touchpad(
    device: &mut evdev::Device,
    grabbed: &mut bool,
    decision_log: &DecisionLog,
    reason: &str,
) {
    if !*grabbed {
        return;
    }

    match device.ungrab() {
        Ok(()) => {
            *grabbed = false;
            decision_log.line(format!("TOUCHPAD_UNGRAB reason={}", reason));
            log::debug!("Touchpad ungrabbed: {}", reason);
        }
        Err(e) => {
            decision_log.line(format!(
                "TOUCHPAD_UNGRAB_FAILED reason={} error={}",
                reason, e
            ));
            log::warn!("Could not ungrab touchpad: {}", e);
        }
    }
}

pub fn run_listener(
    mut device: evdev::Device,
    tx: mpsc::Sender<MomentumMessage>,
    status_rx: mpsc::Receiver<EngineStatus>,
    args: &ResolvedArgs,
    click_inhibit: Arc<AtomicBool>,
    decision_log: DecisionLog,
) {
    let stale_us = args.velocity_stale_ms * 1000;
    let stop_touch_us = args.stop_touch_ms.saturating_mul(1_000);
    if let Err(e) = set_nonblocking(&device) {
        log::warn!("Could not set touchpad fd nonblocking: {}", e);
    }

    let mut state = ListenerState::Idle;
    let mut ptr_x: i32 = 0;
    let mut ptr_y: i32 = 0;
    let mut touch_start_us: u64 = 0;
    let mut click_seen = false;
    let mut awaiting_momentum_stop_touch = false;
    let mut momentum_stop_armed_after_us: u64 = 0;
    let retouch_active_token = Arc::new(AtomicU64::new(0));
    let retouch_stopped_token = Arc::new(AtomicU64::new(0));
    let mut retouch_start_us: u64 = 0;
    let mut current_retouch_token: u64 = 0;
    let mut retouch_ptr_x: i32 = 0;
    let mut retouch_ptr_y: i32 = 0;
    let mut retouch_motion_forwarded = false;
    let mut motion = MotionRing::new();
    let mut touchpad_grabbed = false;
    let mut momentum_active = false;

    loop {
        while let Ok(status) = status_rx.try_recv() {
            match status {
                EngineStatus::PointerActive => {
                    momentum_active = true;
                }
                EngineStatus::PointerIdle => {
                    momentum_active = false;
                    if state != ListenerState::MomentumRetouch {
                        awaiting_momentum_stop_touch = false;
                        ungrab_touchpad(
                            &mut device,
                            &mut touchpad_grabbed,
                            &decision_log,
                            "engine_idle",
                        );
                    }
                }
            }
        }

        if release_stale_grab(
            &mut device,
            &mut touchpad_grabbed,
            state,
            momentum_active,
            &decision_log,
        ) {
            awaiting_momentum_stop_touch = false;
        }

        let events = match device.fetch_events() {
            Ok(events) => events,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(time::Duration::from_millis(2));
                continue;
            }
            Err(e) => {
                log::info!("Touchpad event stream ended: {}", e);
                break;
            }
        };

        let mut grab_actions = Vec::new();

        for event in events {
            let current_ts = event.timestamp();
            let now_us = timestamp_to_us(current_ts);
            log::trace!("Event: {:?} = {}", event.kind(), event.value());

            match event.kind() {
                InputEventKind::AbsAxis(axis) => match axis {
                    AbsoluteAxisType::ABS_X => {
                        let new_x = event.value();
                        if state == ListenerState::MomentumRetouch {
                            let dx = new_x - retouch_ptr_x;
                            retouch_ptr_x = new_x;
                            if dx != 0
                                && retouch_is_stopped(current_retouch_token, &retouch_stopped_token)
                            {
                                mark_retouch_motion_forwarded(
                                    &mut retouch_motion_forwarded,
                                    &decision_log,
                                    dx,
                                    0,
                                );
                                let _ = tx.send(MomentumMessage::ContinuePointer { dx, dy: 0 });
                            }
                        }
                        ptr_x = new_x;
                        if state == ListenerState::OneFingerMove {
                            motion.push(ptr_x, ptr_y, now_us);
                        }
                    }
                    AbsoluteAxisType::ABS_Y => {
                        let new_y = event.value();
                        if state == ListenerState::MomentumRetouch {
                            let dy = new_y - retouch_ptr_y;
                            retouch_ptr_y = new_y;
                            if dy != 0
                                && retouch_is_stopped(current_retouch_token, &retouch_stopped_token)
                            {
                                mark_retouch_motion_forwarded(
                                    &mut retouch_motion_forwarded,
                                    &decision_log,
                                    0,
                                    dy,
                                );
                                let _ = tx.send(MomentumMessage::ContinuePointer { dx: 0, dy });
                            }
                        }
                        ptr_y = new_y;
                        if state == ListenerState::OneFingerMove {
                            motion.push(ptr_x, ptr_y, now_us);
                        }
                    }
                    _ => {}
                },
                InputEventKind::Key(key) => match key {
                    Key::BTN_TOUCH => {
                        if event.value() == 1 {
                            if awaiting_momentum_stop_touch || (touchpad_grabbed && momentum_active)
                            {
                                let armed_after_us = if awaiting_momentum_stop_touch {
                                    momentum_stop_armed_after_us
                                } else {
                                    now_us
                                };
                                let arm_remaining_us = armed_after_us.saturating_sub(now_us);
                                if now_us < armed_after_us {
                                    decision_log.line(format!(
                                        "RETOUCH_IGNORED source=BTN_TOUCH reason=arm_delay remaining_us={}",
                                        arm_remaining_us
                                    ));
                                    log::debug!(
                                        "Ignoring immediate post-lift finger event: {}us before retouch arm",
                                        arm_remaining_us
                                    );
                                    continue;
                                }
                                retouch_start_us = now_us;
                                current_retouch_token =
                                    retouch_active_token.fetch_add(1, Ordering::SeqCst) + 1;
                                decision_log.line(format!(
                                    "RETOUCH_PENDING source=BTN_TOUCH reason=finger_down armed_late_by_us={} confirm_us={}",
                                    now_us.saturating_sub(armed_after_us),
                                    stop_touch_us
                                ));
                                arm_retouch_confirm_timer(
                                    tx.clone(),
                                    decision_log.clone(),
                                    Arc::clone(&retouch_active_token),
                                    Arc::clone(&retouch_stopped_token),
                                    current_retouch_token,
                                    stop_touch_us,
                                );
                                awaiting_momentum_stop_touch = false;
                                retouch_ptr_x = ptr_x;
                                retouch_ptr_y = ptr_y;
                                retouch_motion_forwarded = false;
                                state = ListenerState::MomentumRetouch;
                                log::debug!("State -> MomentumRetouch");
                                continue;
                            }

                            let _ = tx.send(MomentumMessage::Stop);
                            state = ListenerState::OneFingerMove;
                            touch_start_us = now_us;
                            click_seen = false;
                            click_inhibit.store(false, Ordering::Relaxed);
                            motion.clear();
                            motion.push(ptr_x, ptr_y, now_us);
                            log::debug!("State -> OneFingerMove via BTN_TOUCH");
                        } else {
                            match state {
                                ListenerState::OneFingerMove => {
                                    let external_click_seen =
                                        click_inhibit.swap(false, Ordering::Relaxed);
                                    if !click_seen && !external_click_seen {
                                        if maybe_start_pointer(
                                            &tx,
                                            args,
                                            &motion,
                                            now_us,
                                            touch_start_us,
                                            stale_us,
                                            &decision_log,
                                        ) {
                                            momentum_active = true;
                                            awaiting_momentum_stop_touch = true;
                                            momentum_stop_armed_after_us =
                                                now_us + RETOUCH_ARM_DELAY_US;
                                            grab_actions.push(GrabAction::Grab("momentum_start"));
                                            log::debug!(
                                                "Retouch stop will arm in {}us",
                                                RETOUCH_ARM_DELAY_US
                                            );
                                        }
                                    } else if click_seen || external_click_seen {
                                        decision_log.line(format!(
                                            "REJECT reason=click_seen click_seen={} external_click_seen={} touch_us={} ring_samples={} total_samples={} total_dist={:.1}",
                                            click_seen,
                                            external_click_seen,
                                            now_us.saturating_sub(touch_start_us),
                                            motion.count(),
                                            motion.total_count(),
                                            motion.total_distance()
                                        ));
                                        log::debug!("Pointer inertia suppressed after click");
                                    }
                                    state = ListenerState::Idle;
                                }
                                ListenerState::MomentumRetouch => {
                                    let duration_us = now_us.saturating_sub(retouch_start_us);
                                    let already_stopped = current_retouch_token != 0
                                        && retouch_stopped_token.load(Ordering::SeqCst)
                                            == current_retouch_token;
                                    retouch_active_token.fetch_add(1, Ordering::SeqCst);
                                    if duration_us < stop_touch_us && !already_stopped {
                                        decision_log.line(format!(
                                            "RETOUCH_IGNORED source=BTN_TOUCH reason=short_touch duration_us={} confirm_us={}",
                                            duration_us,
                                            stop_touch_us
                                        ));
                                        log::debug!(
                                            "Momentum retouch ignored: short touch {}us",
                                            duration_us
                                        );
                                        if momentum_active {
                                            awaiting_momentum_stop_touch = true;
                                            momentum_stop_armed_after_us = now_us;
                                        }
                                    } else if already_stopped {
                                        decision_log.line(format!(
                                            "RETOUCH_RELEASE reason=after_stop duration_us={} confirm_us={}",
                                            duration_us,
                                            stop_touch_us
                                        ));
                                        log::debug!(
                                            "Momentum retouch released after stop: {}us",
                                            duration_us
                                        );
                                        momentum_active = false;
                                        awaiting_momentum_stop_touch = false;
                                        grab_actions
                                            .push(GrabAction::Ungrab("retouch_release_after_stop"));
                                    } else {
                                        let _ = tx.send(MomentumMessage::Stop);
                                        retouch_stopped_token
                                            .store(current_retouch_token, Ordering::SeqCst);
                                        decision_log.line(format!(
                                            "RETOUCH_STOP reason=confirmed_on_release duration_us={} confirm_us={}",
                                            duration_us,
                                            stop_touch_us
                                        ));
                                        log::debug!(
                                            "Momentum retouch stopped on release: {}us",
                                            duration_us
                                        );
                                        momentum_active = false;
                                        awaiting_momentum_stop_touch = false;
                                        grab_actions.push(GrabAction::Ungrab(
                                            "retouch_confirmed_on_release",
                                        ));
                                    }
                                    current_retouch_token = 0;
                                    retouch_motion_forwarded = false;
                                    state = ListenerState::Idle;
                                }
                                ListenerState::Idle => {}
                            }
                        }
                    }
                    Key::BTN_LEFT | Key::BTN_RIGHT | Key::BTN_MIDDLE => {
                        if event.value() == 1 {
                            click_seen = true;
                            click_inhibit.store(true, Ordering::Relaxed);
                            let _ = tx.send(MomentumMessage::Stop);
                            log::debug!("Click seen, suppressing inertia for this touch");
                        }
                    }
                    Key::BTN_TOOL_DOUBLETAP
                    | Key::BTN_TOOL_TRIPLETAP
                    | Key::BTN_TOOL_QUADTAP
                    | Key::BTN_TOOL_QUINTTAP => {
                        if event.value() == 1 {
                            click_seen = true;
                            retouch_active_token.fetch_add(1, Ordering::SeqCst);
                            current_retouch_token = 0;
                            retouch_motion_forwarded = false;
                            state = ListenerState::Idle;
                            let _ = tx.send(MomentumMessage::Stop);
                            log::debug!("Multitouch gesture -> Stop");
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        if state == ListenerState::MomentumRetouch {
            click_seen = true;
        }

        for action in grab_actions {
            match action {
                GrabAction::Grab(reason) => {
                    grab_touchpad(&mut device, &mut touchpad_grabbed, &decision_log, reason);
                }
                GrabAction::Ungrab(reason) => {
                    ungrab_touchpad(&mut device, &mut touchpad_grabbed, &decision_log, reason);
                }
            }
        }

        if release_stale_grab(
            &mut device,
            &mut touchpad_grabbed,
            state,
            momentum_active,
            &decision_log,
        ) {
            awaiting_momentum_stop_touch = false;
        }
    }

    ungrab_touchpad(
        &mut device,
        &mut touchpad_grabbed,
        &decision_log,
        "listener_exit",
    );
}

pub fn run_button_listener(
    mut device: evdev::Device,
    tx: mpsc::Sender<MomentumMessage>,
    click_inhibit: Arc<AtomicBool>,
    decision_log: DecisionLog,
) {
    while let Ok(events) = device.fetch_events() {
        for event in events {
            let InputEventKind::Key(key) = event.kind() else {
                continue;
            };
            if event.value() != 1 {
                continue;
            }
            if matches!(key, Key::BTN_LEFT | Key::BTN_RIGHT | Key::BTN_MIDDLE) {
                click_inhibit.store(true, Ordering::Relaxed);
                let _ = tx.send(MomentumMessage::Stop);
                decision_log.line(format!("BUTTON_CLICK key={:?} action=stop_inhibit", key));
                log::debug!("Touchpad button click seen on mouse node: {:?}", key);
            }
        }
    }

    log::info!("Touchpad button event stream ended");
}

fn maybe_start_pointer(
    tx: &mpsc::Sender<MomentumMessage>,
    args: &ResolvedArgs,
    motion: &MotionRing,
    now_us: u64,
    touch_start_us: u64,
    stale_us: u64,
    decision_log: &DecisionLog,
) -> bool {
    let touch_duration_us = now_us.saturating_sub(touch_start_us);
    let summary = motion.motion_summary();
    if let Some(summary) = summary {
        log::debug!(
            "Pointer lift summary: ring_samples={} total_samples={} touch={}us motion={}us dx={} dy={} recent_dist={:.1} total_dist={:.1}",
            motion.count(),
            motion.total_count(),
            touch_duration_us,
            summary.duration_us,
            summary.dx,
            summary.dy,
            summary.distance,
            motion.total_distance()
        );
    } else {
        log::debug!(
            "Pointer lift summary: ring_samples={} total_samples={} touch={}us total_dist={:.1}",
            motion.count(),
            motion.total_count(),
            touch_duration_us,
            motion.total_distance()
        );
    }

    let context = decision_context(motion, touch_duration_us, summary);

    if touch_duration_us < MIN_TOUCH_US {
        decision_log.line(format!(
            "REJECT reason=touch_too_short {} min_touch_us={}",
            context, MIN_TOUCH_US
        ));
        log::debug!(
            "Pointer lift ignored: touch too short touch={}us threshold={}us",
            touch_duration_us,
            MIN_TOUCH_US
        );
        return false;
    }

    if motion.total_distance() < MIN_MOTION_DISTANCE {
        decision_log.line(format!(
            "REJECT reason=movement_too_short {} min_total_dist={:.1}",
            context, MIN_MOTION_DISTANCE
        ));
        log::debug!(
            "Pointer lift ignored: movement too short total_dist={:.1} threshold={:.1}",
            motion.total_distance(),
            MIN_MOTION_DISTANCE
        );
        return false;
    }

    let Some(release) =
        motion.compute_release_velocity(now_us, stale_us, args.pointer_min_velocity)
    else {
        decision_log.line(format!(
            "REJECT reason=no_fresh_velocity {} stale_us={}",
            context, stale_us
        ));
        log::debug!("Pointer lift ignored: no fresh velocity");
        return false;
    };

    let vx = release.vx;
    let vy = release.vy;
    let speed = (vx * vx + vy * vy).sqrt();
    let velocity_context = release_velocity_context(&release);
    if speed < args.pointer_min_velocity {
        decision_log.line(format!(
            "REJECT reason=velocity_too_low {} {} vx={:.1} vy={:.1} speed={:.1} min_speed={:.1}",
            context, velocity_context, vx, vy, speed, args.pointer_min_velocity
        ));
        log::debug!(
            "Pointer too slow: vx={:.1} vy={:.1} speed={:.1} threshold={:.1}",
            vx,
            vy,
            speed,
            args.pointer_min_velocity
        );
        return false;
    }

    let start_vx = vx * args.pointer_start_speed_multiplier;
    let start_vy = vy * args.pointer_start_speed_multiplier;
    let start_speed = speed * args.pointer_start_speed_multiplier;
    decision_log.line(format!(
        "START reason=criteria_met {} {} release_vx={:.1} release_vy={:.1} release_speed={:.1} start_vx={:.1} start_vy={:.1} start_speed={:.1} start_multiplier={:.3} min_touch_us={} min_total_dist={:.1} min_speed={:.1}",
        context,
        velocity_context,
        vx,
        vy,
        speed,
        start_vx,
        start_vy,
        start_speed,
        args.pointer_start_speed_multiplier,
        MIN_TOUCH_US,
        MIN_MOTION_DISTANCE,
        args.pointer_min_velocity
    ));
    log::debug!(
        "Pointer inertia: release_speed={:.1} start_speed={:.1} multiplier={:.3}",
        speed,
        start_speed,
        args.pointer_start_speed_multiplier
    );
    let _ = tx.send(MomentumMessage::StartPointer {
        vx: start_vx,
        vy: start_vy,
    });
    true
}

fn decision_context(
    motion: &MotionRing,
    touch_duration_us: u64,
    summary: Option<MotionSummary>,
) -> String {
    let base = format!(
        "touch_us={} ring_samples={} total_samples={} total_dist={:.1}",
        touch_duration_us,
        motion.count(),
        motion.total_count(),
        motion.total_distance()
    );

    match summary {
        Some(summary) => format!(
            "{} recent_samples={} motion_us={} dx={} dy={} recent_dist={:.1} recent_path={:.1}",
            base,
            summary.samples,
            summary.duration_us,
            summary.dx,
            summary.dy,
            summary.distance,
            summary.path_distance
        ),
        None => base,
    }
}

fn release_velocity_context(release: &ReleaseVelocity) -> String {
    let mut context = format!(
        "velocity_samples={} velocity_us={} velocity_dx={} velocity_dy={} velocity_dist={:.1} velocity_path={:.1} velocity_age_us={}",
        release.summary.samples,
        release.summary.duration_us,
        release.summary.dx,
        release.summary.dy,
        release.summary.distance,
        release.summary.path_distance,
        release.age_us
    );

    if let Some(trim) = release.trim {
        context.push_str(&format!(
            " lift_tail_trimmed=true tail_samples={} tail_speed={:.1} previous_speed={:.1} trim_threshold={:.1}",
            trim.tail_samples,
            trim.tail_speed,
            trim.previous_speed,
            trim.trim_threshold
        ));
    } else {
        context.push_str(" lift_tail_trimmed=false");
    }

    context
}

fn arm_retouch_confirm_timer(
    tx: mpsc::Sender<MomentumMessage>,
    decision_log: DecisionLog,
    active_token: Arc<AtomicU64>,
    stopped_token: Arc<AtomicU64>,
    token: u64,
    stop_touch_us: u64,
) {
    thread::spawn(move || {
        thread::sleep(time::Duration::from_micros(stop_touch_us));
        if active_token.load(Ordering::SeqCst) != token {
            return;
        }

        let _ = tx.send(MomentumMessage::Stop);
        stopped_token.store(token, Ordering::SeqCst);
        decision_log.line(format!(
            "RETOUCH_STOP reason=confirmed_by_timer duration_us={} confirm_us={}",
            stop_touch_us, stop_touch_us
        ));
    });
}

fn retouch_is_stopped(current_token: u64, stopped_token: &AtomicU64) -> bool {
    current_token != 0 && stopped_token.load(Ordering::SeqCst) == current_token
}

fn mark_retouch_motion_forwarded(
    already_forwarded: &mut bool,
    decision_log: &DecisionLog,
    raw_dx: i32,
    raw_dy: i32,
) {
    if *already_forwarded {
        return;
    }

    *already_forwarded = true;
    decision_log.line(format!(
        "RETOUCH_CONTINUE reason=motion_after_stop raw_dx={} raw_dy={}",
        raw_dx, raw_dy
    ));
    log::debug!("Forwarding continued retouch motion after inertia stop");
}

fn release_stale_grab(
    device: &mut evdev::Device,
    grabbed: &mut bool,
    state: ListenerState,
    momentum_active: bool,
    decision_log: &DecisionLog,
) -> bool {
    if !*grabbed || momentum_active || state == ListenerState::MomentumRetouch {
        return false;
    }

    ungrab_touchpad(device, grabbed, decision_log, "inactive_listener_state");
    true
}
