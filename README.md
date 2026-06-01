# rinertia pointer inertia

A small Linux userspace daemon that adds pointer inertia to touchpads.

This is a modified/forked version of `rinertia`. The original project focused
on momentum scrolling. This version removes the scroll path and keeps only
pointer/cursor inertia, intended for systems where the native touchpad driver
already provides good inertial scrolling but no inertial pointer movement.

The code is experimental and device-dependent. It has been tuned on one laptop
touchpad, so expect to adjust the parameters for other hardware.

## What it does

- Passively reads touchpad events from `/dev/input/event*` through evdev.
- Creates a uinput virtual relative pointer device.
- Starts cursor inertia after a qualifying one-finger movement.
- Stops inertia on a real click, multitouch gesture, or confirmed retouch.
- Ignores very short post-lift retouches to avoid false stops caused by finger
  bounce or skin elasticity.
- Temporarily grabs the real touchpad only while pointer inertia is active, so
  the touch used to stop inertia is hidden from the normal touchpad driver.

It does not implement scrolling. If your existing Synaptics/libinput setup
already has good scroll behavior, this daemon leaves that path alone.

## How it works

```text
touchpad evdev device
        |
        | normal movement: passive read
        | active inertia: temporary EVIOCGRAB
        v
   rinertia listener ----> pointer momentum engine
                                  |
                                  v
                         uinput virtual mouse
                                  |
                                  v
                         desktop input stack
```

The real touchpad driver still handles normal cursor movement, tapping,
clicking, dragging, and scrolling. `rinertia` injects extra relative pointer
movement after the finger is lifted.

While inertia is active, `rinertia` temporarily grabs the real touchpad with
`EVIOCGRAB`. This prevents the normal touchpad driver from seeing the finger
touch used to stop inertia. The grab is released when inertia ends or when the
stop touch is released. If the process exits, the kernel releases the grab.

## Build

Install Rust, then build the release binary:

```bash
cargo build --release
```

For a system install:

```bash
sudo install -m 0755 target/release/rinertia /usr/local/bin/rinertia
```

## Install script

For a normal system install, run:

```bash
./install.sh
```

The script builds the release binary, installs it as
`/usr/local/bin/rinertia`, installs the sample config as
`/etc/rinertia/config.toml` if no config exists yet, installs the udev rule,
and enables an autostart service.

It detects `systemd` or SysV init automatically. You can force one mode:

```bash
INIT_STYLE=systemd ./install.sh
INIT_STYLE=sysv ./install.sh
INIT_STYLE=both ./install.sh
```

To install files without starting/restarting the daemon:

```bash
START_SERVICE=0 ./install.sh
```

## Quick test

Run with the currently tested parameters:

```bash
sudo /usr/local/bin/rinertia \
  --pointer-drag 0.01 \
  --pointer-min-velocity 100 \
  --log-level info
```

For diagnostics:

```bash
sudo /usr/local/bin/rinertia \
  --pointer-drag 0.01 \
  --pointer-min-velocity 100 \
  --log-level debug \
  --decision-log /tmp/rinertia-decisions.log
```

The decision log records why inertia starts or is rejected, and how much
virtual movement was emitted (`total_dx`, `total_dy`).

## Configuration

Copy the example config:

```bash
mkdir -p ~/.config/rinertia
cp dist/config.toml.example ~/.config/rinertia/config.toml
```

Run with:

```bash
sudo /usr/local/bin/rinertia --config ~/.config/rinertia/config.toml
```

Important parameters:

| Parameter | Meaning |
| --- | --- |
| `pointer.drag` | Higher value means faster decay and shorter inertia. |
| `pointer.speed_factor` | Converts touchpad units to virtual mouse movement. |
| `pointer.min_velocity` | Minimum release velocity required to start inertia. |
| `pointer.velocity_stale_ms` | Maximum age of release samples used for velocity. |
| `decision_log` | Optional path for start/reject diagnostics. |
| `log_level` | `error`, `warn`, `info`, `debug`, or `trace`. |

## udev permissions

The daemon needs read access to the touchpad event device and write access to
`/dev/uinput`. Running as root is the simplest test path.

For non-root use, install the sample udev rule:

```bash
sudo install -m 0644 dist/99-rinertia.rules /etc/udev/rules.d/99-rinertia.rules
sudo udevadm control --reload-rules
sudo udevadm trigger
```

Then make sure your user is in the required input/uinput-capable group for your
distribution. On many systems this means the `input` group.

## systemd user service

After installing the binary and config:

```bash
mkdir -p ~/.config/systemd/user
cp dist/rinertia.service ~/.config/systemd/user/rinertia.service
systemctl --user daemon-reload
systemctl --user enable --now rinertia.service
```

If your system does not run user services in the graphical session, start the
binary manually or adapt the service file to your distribution.

## Safety notes

Pointer inertia can interact badly with drag-and-drop if false clicks or false
retouches are accepted. This fork therefore suppresses inertia after a click
and requires a confirmed retouch before stopping active inertia. During active
inertia it also temporarily grabs the real touchpad so that a stop touch is not
processed by the normal touchpad driver as a click or drag.

The current behavior is tuned for practical desktop use, not for strict input
correctness. Test carefully before using it on machines where accidental
drag-and-drop would be costly.

## Origin and license

This is a modified version/fork of the original MIT-licensed `rinertia` project
by JimMoen. The MIT license text is preserved in `LICENSE`.

The main changes in this fork are:

- removed scroll momentum code;
- kept only pointer inertia;
- added touch/click safety filters;
- added temporary `EVIOCGRAB` while pointer inertia is active;
- added decision logging for start/reject/stop diagnostics;
- tuned behavior for Synaptics-style touchpad setups that already provide
  native inertial scrolling.

## License

MIT
