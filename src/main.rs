use anyhow::Result;
use clap::Parser;
use log;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::thread;

mod config;
mod decision_log;
mod device_discovery;
mod instance_lock;
mod momentum;
mod touchpad;
mod virtual_device;
mod x11_pointer;

/// Pointer inertia daemon for Linux touchpads.
///
/// Passively monitors touchpad events (no device grab) and injects
/// momentum pointer events via a virtual uinput device.
#[derive(Parser, Debug, Clone)]
#[command(name = "rinertia", version, about)]
pub struct Args {
    /// Path to TOML config file
    #[arg(short, long)]
    pub config: Option<String>,

    /// Touchpad device path (auto-detect if omitted)
    #[arg(short, long)]
    pub device: Option<String>,

    /// Match touchpad by name substring (e.g. "ELAN", "Synaptics")
    #[arg(short = 'n', long)]
    pub device_name: Option<String>,

    /// Max age (ms) of velocity samples; older samples are discarded (default: 150)
    #[arg(long)]
    pub velocity_stale_ms: Option<u64>,

    /// Drag coefficient for pointer inertia (0.0 ~ 1.0)
    #[arg(long)]
    pub pointer_drag: Option<f64>,

    /// Scale factor from touchpad units to virtual mouse units
    #[arg(long)]
    pub pointer_speed_factor: Option<f64>,

    /// Minimum touchpad speed to trigger pointer inertia
    #[arg(long)]
    pub pointer_min_velocity: Option<f64>,

    /// Maximum pointer inertia duration in milliseconds (0 disables the limit)
    #[arg(long)]
    pub pointer_max_duration_ms: Option<u64>,

    /// Touch duration in milliseconds required to stop active inertia
    #[arg(long)]
    pub stop_touch_ms: Option<u64>,

    /// Dry mode: don't create virtual device, only log
    #[arg(long)]
    pub dry: bool,

    /// Append inertia start/reject decisions to this file
    #[arg(long)]
    pub decision_log: Option<String>,

    /// Log level (off, error, warn, info, debug, trace)
    #[arg(long)]
    pub log_level: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedArgs {
    pub device: Option<String>,
    pub device_name: Option<String>,
    pub velocity_stale_ms: u64,
    pub pointer_drag: f64,
    pub pointer_speed_factor: f64,
    pub pointer_min_velocity: f64,
    pub pointer_max_duration_ms: u64,
    pub stop_touch_ms: u64,
    pub dry: bool,
    pub decision_log: Option<String>,
    pub log_level: String,
}

#[derive(Debug, Clone)]
pub enum MomentumMessage {
    StartPointer { vx: f64, vy: f64 },
    Stop,
}

#[derive(Debug, Clone, Copy)]
pub enum EngineStatus {
    PointerActive,
    PointerIdle,
}

fn main() -> Result<()> {
    let cli = Args::parse();

    let (cfg, config_missing) = match &cli.config {
        Some(path) => {
            let p = std::path::Path::new(path);
            if p.exists() {
                (config::load(p)?, false)
            } else {
                (config::Config::default(), true)
            }
        }
        None => (config::Config::default(), false),
    };

    let args = config::resolve(&cli, &cfg);

    env_logger::Builder::new()
        .filter_module(
            "rinertia",
            args.log_level.parse().unwrap_or(log::LevelFilter::Info),
        )
        .parse_default_env()
        .init();

    let _instance_lock = instance_lock::acquire()?;

    if config_missing {
        log::error!("Config file not found: {}", cli.config.as_deref().unwrap());
        log::warn!("Falling back to built-in defaults");
    }

    if cli.config.is_some() && !config_missing {
        log::info!("Loaded config: {}", cli.config.as_deref().unwrap());
    }

    let decision_log = match &args.decision_log {
        Some(path) => {
            log::info!("Decision log: {}", path);
            decision_log::DecisionLog::open(std::path::Path::new(path))?
        }
        None => decision_log::DecisionLog::disabled(),
    };

    let touchpad_path =
        device_discovery::find_touchpad(args.device.as_deref(), args.device_name.as_deref())?;
    log::info!("Using touchpad: {}", touchpad_path.display());

    let vdev = if args.dry {
        log::info!("Dry mode: no virtual device created");
        None
    } else {
        let uinput_path = std::path::Path::new("/dev/uinput");
        if let Err(e) = std::fs::OpenOptions::new().write(true).open(uinput_path) {
            log::error!(
                "/dev/uinput: {}. Add your user to the 'uinput' group, or run as root. See dist/99-rinertia.rules.",
                e
            );
            std::process::exit(1);
        }
        Some(virtual_device::VirtualDevice::new()?)
    };

    let (tx, rx) = mpsc::channel::<MomentumMessage>();
    let (status_tx, status_rx) = mpsc::channel::<EngineStatus>();

    let tp_device = evdev::Device::open(&touchpad_path)?;
    let tp_phys = device_discovery::get_phys(&tp_device);
    log::info!(
        "Touchpad: {} [phys: {}]",
        tp_device.name().unwrap_or("unknown"),
        tp_phys.as_deref().unwrap_or("?")
    );

    let click_inhibit = Arc::new(AtomicBool::new(false));
    let button_thread =
        device_discovery::find_touchpad_button_device(&touchpad_path, tp_phys.as_deref())
            .map(|(_path, button_device)| {
                let tx_button = tx.clone();
                let click_inhibit_button = click_inhibit.clone();
                let decision_log_button = decision_log.clone();
                thread::Builder::new()
                    .name("button-listener".into())
                    .spawn(move || {
                        touchpad::run_button_listener(
                            button_device,
                            tx_button,
                            click_inhibit_button,
                            decision_log_button,
                        );
                    })
            })
            .transpose()?;

    let args_clone = args.clone();
    let click_inhibit_listener = click_inhibit.clone();
    let decision_log_listener = decision_log.clone();
    let touchpad_thread = thread::Builder::new()
        .name("listener".into())
        .spawn(move || {
            touchpad::run_listener(
                tp_device,
                tx,
                status_rx,
                &args_clone,
                click_inhibit_listener,
                decision_log_listener,
            );
        })?;

    log::info!("Momentum engine started");
    momentum::run_engine(rx, status_tx, vdev, &args, decision_log);

    let _ = touchpad_thread.join();
    if let Some(t) = button_thread {
        let _ = t.join();
    }

    Ok(())
}
