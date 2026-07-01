use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::decision_log::DecisionLog;
use crate::virtual_device::VirtualDevice;
use crate::x11_pointer::PointerBlockDetector;
use crate::{EngineStatus, MomentumMessage};

const POINTER_TICK: Duration = Duration::from_micros(8_000);
const POINTER_STOP_SPEED: f64 = 25.0;
const NOMINAL_FRAME_SEC: f64 = 1.0 / 60.0;

#[derive(Debug, Clone, Copy, PartialEq)]
enum EngineState {
    Idle,
    PointerMomentum,
}

pub fn run_engine(
    rx: mpsc::Receiver<MomentumMessage>,
    status_tx: mpsc::Sender<EngineStatus>,
    mut vdev: Option<VirtualDevice>,
    args: &crate::ResolvedArgs,
    decision_log: DecisionLog,
) {
    let pointer_tau_ms = drag_to_tau_ms(args.pointer_drag);
    let pointer_speed_factor = args.pointer_speed_factor;
    let continued_pointer_scale = pointer_speed_factor / NOMINAL_FRAME_SEC;

    let mut state = EngineState::Idle;
    let mut vx: f64 = 0.0;
    let mut vy: f64 = 0.0;
    let mut x_accum: f64 = 0.0;
    let mut y_accum: f64 = 0.0;
    let mut last_tick = Instant::now();
    let mut momentum_started_at = Instant::now();
    let mut emit_count: u64 = 0;
    let mut total_dx: i64 = 0;
    let mut total_dy: i64 = 0;
    let mut pointer_block_detector = PointerBlockDetector::new();

    loop {
        match state {
            EngineState::Idle => {
                let msg = match rx.recv() {
                    Ok(m) => m,
                    Err(_) => {
                        log::info!("Channel closed, engine shutting down");
                        return;
                    }
                };

                match msg {
                    MomentumMessage::StartPointer { vx: pvx, vy: pvy } => {
                        vx = pvx;
                        vy = pvy;
                        x_accum = 0.0;
                        y_accum = 0.0;
                        emit_count = 0;
                        total_dx = 0;
                        total_dy = 0;
                        last_tick = Instant::now();
                        momentum_started_at = last_tick;
                        pointer_block_detector.reset();
                        state = EngineState::PointerMomentum;
                        send_status(&status_tx, EngineStatus::PointerActive);
                        decision_log.line(format!(
                            "ENGINE_START vx={:.1} vy={:.1} speed={:.1}",
                            vx,
                            vy,
                            (vx * vx + vy * vy).sqrt()
                        ));
                        log::debug!("PointerMomentum start: vx={:.1} vy={:.1}", vx, vy);
                    }
                    MomentumMessage::ContinuePointer { dx, dy } => {
                        emit_continued_pointer(
                            &mut vdev,
                            &mut x_accum,
                            &mut y_accum,
                            dx,
                            dy,
                            continued_pointer_scale,
                        );
                    }
                    MomentumMessage::Stop => {
                        x_accum = 0.0;
                        y_accum = 0.0;
                    }
                }
            }

            EngineState::PointerMomentum => {
                let msg = rx.recv_timeout(POINTER_TICK);
                match msg {
                    Ok(MomentumMessage::Stop) => {
                        log::debug!("PointerMomentum interrupted by Stop");
                        vx = 0.0;
                        vy = 0.0;
                        x_accum = 0.0;
                        y_accum = 0.0;
                        decision_log.line(format!(
                            "ENGINE_STOP reason=message emit_count={} total_dx={} total_dy={}",
                            emit_count, total_dx, total_dy
                        ));
                        state = EngineState::Idle;
                        send_status(&status_tx, EngineStatus::PointerIdle);
                        continue;
                    }
                    Ok(MomentumMessage::StartPointer {
                        vx: new_vx,
                        vy: new_vy,
                    }) => {
                        vx = new_vx;
                        vy = new_vy;
                        x_accum = 0.0;
                        y_accum = 0.0;
                        emit_count = 0;
                        total_dx = 0;
                        total_dy = 0;
                        last_tick = Instant::now();
                        momentum_started_at = last_tick;
                        pointer_block_detector.reset();
                        decision_log.line(format!(
                            "ENGINE_RESTART vx={:.1} vy={:.1} speed={:.1}",
                            vx,
                            vy,
                            (vx * vx + vy * vy).sqrt()
                        ));
                        send_status(&status_tx, EngineStatus::PointerActive);
                        continue;
                    }
                    Ok(MomentumMessage::ContinuePointer { dx, dy }) => {
                        vx = 0.0;
                        vy = 0.0;
                        x_accum = 0.0;
                        y_accum = 0.0;
                        decision_log.line(format!(
                            "ENGINE_STOP reason=continued_touch emit_count={} total_dx={} total_dy={}",
                            emit_count, total_dx, total_dy
                        ));
                        state = EngineState::Idle;
                        send_status(&status_tx, EngineStatus::PointerIdle);
                        emit_continued_pointer(
                            &mut vdev,
                            &mut x_accum,
                            &mut y_accum,
                            dx,
                            dy,
                            continued_pointer_scale,
                        );
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        log::info!("Channel closed, engine shutting down");
                        return;
                    }
                }

                let now = Instant::now();
                let dt = now.duration_since(last_tick).as_secs_f64();
                last_tick = now;

                if pointer_block_detector.pointer_is_blocked() {
                    vx = 0.0;
                    vy = 0.0;
                    x_accum = 0.0;
                    y_accum = 0.0;
                    decision_log.line(format!(
                        "ENGINE_DONE reason=pointer_blocked emit_count={} total_dx={} total_dy={}",
                        emit_count, total_dx, total_dy
                    ));
                    log::debug!("PointerMomentum stop: X11 pointer is blocked");
                    state = EngineState::Idle;
                    send_status(&status_tx, EngineStatus::PointerIdle);
                    continue;
                }

                let elapsed = now.duration_since(momentum_started_at);
                if args.pointer_max_duration_ms > 0
                    && elapsed >= Duration::from_millis(args.pointer_max_duration_ms)
                {
                    vx = 0.0;
                    vy = 0.0;
                    x_accum = 0.0;
                    y_accum = 0.0;
                    decision_log.line(format!(
                        "ENGINE_DONE reason=max_duration elapsed_ms={} max_duration_ms={} emit_count={} total_dx={} total_dy={}",
                        elapsed.as_millis(),
                        args.pointer_max_duration_ms,
                        emit_count,
                        total_dx,
                        total_dy
                    ));
                    log::debug!(
                        "PointerMomentum stop: maximum duration {}ms",
                        args.pointer_max_duration_ms
                    );
                    state = EngineState::Idle;
                    send_status(&status_tx, EngineStatus::PointerIdle);
                    continue;
                }

                let decay = (-(dt * 1000.0) / pointer_tau_ms).exp();
                vx *= decay;
                vy *= decay;

                let speed = (vx * vx + vy * vy).sqrt();
                if speed < POINTER_STOP_SPEED {
                    log::debug!("PointerMomentum stop: speed={:.1}", speed);
                    vx = 0.0;
                    vy = 0.0;
                    x_accum = 0.0;
                    y_accum = 0.0;
                    decision_log.line(format!(
                        "ENGINE_DONE reason=speed_below_stop speed={:.1} emit_count={} total_dx={} total_dy={}",
                        speed,
                        emit_count,
                        total_dx,
                        total_dy
                    ));
                    state = EngineState::Idle;
                    send_status(&status_tx, EngineStatus::PointerIdle);
                    continue;
                }

                let frame_scale = dt / NOMINAL_FRAME_SEC;
                x_accum += vx * pointer_speed_factor * frame_scale;
                y_accum += vy * pointer_speed_factor * frame_scale;

                let dx = take_integer_motion(&mut x_accum);
                let dy = take_integer_motion(&mut y_accum);

                if dx != 0 || dy != 0 {
                    emit_count += 1;
                    total_dx += dx as i64;
                    total_dy += dy as i64;
                    if emit_count <= 3 || emit_count % 25 == 0 {
                        decision_log.line(format!(
                            "ENGINE_EMIT n={} dx={} dy={} total_dx={} total_dy={} vx={:.1} vy={:.1} speed={:.1}",
                            emit_count,
                            dx,
                            dy,
                            total_dx,
                            total_dy,
                            vx,
                            vy,
                            speed
                        ));
                    }
                    log::debug!(
                        "PointerMomentum emit: dx={} dy={} vx={:.1} vy={:.1} speed={:.1}",
                        dx,
                        dy,
                        vx,
                        vy,
                        speed
                    );
                    emit_pointer(&mut vdev, dx, dy);
                    if vdev.is_some() {
                        pointer_block_detector.record_emission(dx, dy);
                    }
                }
            }
        }
    }
}

fn send_status(tx: &mpsc::Sender<EngineStatus>, status: EngineStatus) {
    let _ = tx.send(status);
}

fn drag_to_tau_ms(drag: f64) -> f64 {
    let drag = drag.clamp(0.001, 0.95);
    let per_old_tick = 1.0 - drag;
    -16.666_666_667 / per_old_tick.ln()
}

fn take_integer_motion(accum: &mut f64) -> i32 {
    let whole = accum.trunc();
    *accum -= whole;
    whole as i32
}

fn emit_pointer(vdev: &mut Option<VirtualDevice>, dx: i32, dy: i32) {
    match vdev {
        Some(dev) => {
            if let Err(e) = dev.emit_pointer(dx, dy) {
                log::error!("emit_pointer failed: {}", e);
            }
        }
        None => {
            log::info!("[dry] pointer dx={} dy={}", dx, dy);
        }
    }
}

fn emit_continued_pointer(
    vdev: &mut Option<VirtualDevice>,
    x_accum: &mut f64,
    y_accum: &mut f64,
    raw_dx: i32,
    raw_dy: i32,
    scale: f64,
) {
    let (dx, dy) = take_continued_motion(x_accum, y_accum, raw_dx, raw_dy, scale);
    if dx == 0 && dy == 0 {
        return;
    }

    log::trace!(
        "Continued touch pointer: raw_dx={} raw_dy={} dx={} dy={}",
        raw_dx,
        raw_dy,
        dx,
        dy
    );
    emit_pointer(vdev, dx, dy);
}

fn take_continued_motion(
    x_accum: &mut f64,
    y_accum: &mut f64,
    raw_dx: i32,
    raw_dy: i32,
    scale: f64,
) -> (i32, i32) {
    *x_accum += raw_dx as f64 * scale;
    *y_accum += raw_dy as f64 * scale;

    let dx = take_integer_motion(x_accum);
    let dy = take_integer_motion(y_accum);
    (dx, dy)
}

#[cfg(test)]
mod tests {
    use super::take_continued_motion;

    #[test]
    fn continued_motion_preserves_fractional_distance() {
        let mut x_accum = 0.0;
        let mut y_accum = 0.0;

        assert_eq!(
            take_continued_motion(&mut x_accum, &mut y_accum, 1, -1, 0.5),
            (0, 0)
        );
        assert_eq!(
            take_continued_motion(&mut x_accum, &mut y_accum, 1, -1, 0.5),
            (1, -1)
        );
        assert_eq!(x_accum, 0.0);
        assert_eq!(y_accum, 0.0);
    }
}
