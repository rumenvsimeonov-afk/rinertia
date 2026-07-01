use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    pub device: Option<DeviceConfig>,
    pub pointer: Option<PointerConfig>,
    pub decision_log: Option<String>,
    pub log_level: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct DeviceConfig {
    pub path: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct PointerConfig {
    pub drag: Option<f64>,
    pub speed_factor: Option<f64>,
    pub min_velocity: Option<f64>,
    pub start_speed_multiplier: Option<f64>,
    pub velocity_stale_ms: Option<u64>,
    pub max_duration_ms: Option<u64>,
    pub stop_touch_ms: Option<u64>,
}

pub const DEFAULT_VELOCITY_STALE_MS: u64 = 150;
pub const DEFAULT_POINTER_DRAG: f64 = 0.01;
pub const DEFAULT_POINTER_SPEED_FACTOR: f64 = 0.0075;
pub const DEFAULT_POINTER_MIN_VELOCITY: f64 = 100.0;
pub const DEFAULT_POINTER_START_SPEED_MULTIPLIER: f64 = 1.3;
pub const DEFAULT_POINTER_MAX_DURATION_MS: u64 = 0;
pub const DEFAULT_STOP_TOUCH_MS: u64 = 56;
pub const DEFAULT_LOG_LEVEL: &str = "info";

pub fn load(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)?;
    let config: Config = toml::from_str(&content)?;
    Ok(config)
}

/// Resolve final Args: CLI (if set) > config file > hardcoded defaults.
pub fn resolve(cli: &crate::Args, cfg: &Config) -> crate::ResolvedArgs {
    let dev = cfg.device.as_ref();
    let pointer = cfg.pointer.as_ref();

    crate::ResolvedArgs {
        device: cli
            .device
            .clone()
            .or_else(|| dev.and_then(|d| d.path.clone())),
        device_name: cli
            .device_name
            .clone()
            .or_else(|| dev.and_then(|d| d.name.clone())),
        velocity_stale_ms: cli.velocity_stale_ms.unwrap_or_else(|| {
            pointer
                .and_then(|p| p.velocity_stale_ms)
                .unwrap_or(DEFAULT_VELOCITY_STALE_MS)
        }),
        pointer_drag: cli
            .pointer_drag
            .unwrap_or_else(|| pointer.and_then(|p| p.drag).unwrap_or(DEFAULT_POINTER_DRAG)),
        pointer_speed_factor: cli.pointer_speed_factor.unwrap_or_else(|| {
            pointer
                .and_then(|p| p.speed_factor)
                .unwrap_or(DEFAULT_POINTER_SPEED_FACTOR)
        }),
        pointer_min_velocity: cli.pointer_min_velocity.unwrap_or_else(|| {
            pointer
                .and_then(|p| p.min_velocity)
                .unwrap_or(DEFAULT_POINTER_MIN_VELOCITY)
        }),
        pointer_start_speed_multiplier: cli
            .pointer_start_speed_multiplier
            .or_else(|| pointer.and_then(|p| p.start_speed_multiplier))
            .unwrap_or(DEFAULT_POINTER_START_SPEED_MULTIPLIER)
            .clamp(0.1, 5.0),
        pointer_max_duration_ms: cli.pointer_max_duration_ms.unwrap_or_else(|| {
            pointer
                .and_then(|p| p.max_duration_ms)
                .unwrap_or(DEFAULT_POINTER_MAX_DURATION_MS)
        }),
        stop_touch_ms: cli
            .stop_touch_ms
            .or_else(|| pointer.and_then(|p| p.stop_touch_ms))
            .unwrap_or(DEFAULT_STOP_TOUCH_MS)
            .clamp(10, 2_000),
        dry: cli.dry,
        decision_log: cli
            .decision_log
            .clone()
            .or_else(|| cfg.decision_log.clone()),
        log_level: cli.log_level.clone().unwrap_or_else(|| {
            cfg.log_level
                .clone()
                .unwrap_or_else(|| DEFAULT_LOG_LEVEL.into())
        }),
    }
}
