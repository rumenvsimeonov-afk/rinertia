#!/bin/sh
set -eu

PROJECT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PREFIX=${PREFIX:-/usr/local}
SYSCONFDIR=${SYSCONFDIR:-/etc}
INIT_STYLE=${INIT_STYLE:-auto}
START_SERVICE=${START_SERVICE:-1}

BIN_DIR=$PREFIX/bin
BIN_PATH=$BIN_DIR/rinertia
CONFIG_DIR=$SYSCONFDIR/rinertia
CONFIG_PATH=$CONFIG_DIR/config.toml
UDEV_RULE=/etc/udev/rules.d/99-rinertia.rules

if [ "$(id -u)" -eq 0 ]; then
    SUDO=
else
    SUDO=${SUDO:-sudo}
fi

run_root()
{
    if [ -n "$SUDO" ]; then
        "$SUDO" "$@"
    else
        "$@"
    fi
}

detect_init_style()
{
    if [ "$INIT_STYLE" != "auto" ]; then
        printf '%s\n' "$INIT_STYLE"
        return
    fi

    if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
        printf '%s\n' systemd
        return
    fi

    if command -v update-rc.d >/dev/null 2>&1 && [ -d /etc/init.d ]; then
        printf '%s\n' sysv
        return
    fi

    printf '%s\n' none
}

enable_systemd_service()
{
    if command -v systemctl >/dev/null 2>&1; then
        if [ -d /run/systemd/system ]; then
            run_root systemctl daemon-reload
        fi

        if run_root systemctl enable rinertia.service; then
            return
        fi
    fi

    run_root install -d -m 0755 /etc/systemd/system/multi-user.target.wants
    run_root ln -sf /etc/systemd/system/rinertia.service \
        /etc/systemd/system/multi-user.target.wants/rinertia.service
}

install_systemd_service()
{
    echo "Installing systemd service..."
    run_root install -m 0644 "$PROJECT_DIR/dist/rinertia-system.service" \
        /etc/systemd/system/rinertia.service
    enable_systemd_service
    if [ "$START_SERVICE" = "1" ] && [ -d /run/systemd/system ]; then
        run_root systemctl restart rinertia.service
    fi
}

install_sysv_service()
{
    echo "Installing SysV init script..."
    run_root install -m 0755 "$PROJECT_DIR/dist/rinertia.init" /etc/init.d/rinertia
    run_root update-rc.d rinertia defaults
    if [ "$START_SERVICE" = "1" ] && [ ! -d /run/systemd/system ]; then
        if command -v service >/dev/null 2>&1; then
            run_root service rinertia restart || run_root service rinertia start
        else
            run_root /etc/init.d/rinertia restart || run_root /etc/init.d/rinertia start
        fi
    fi
}

cd "$PROJECT_DIR"

echo "Building rinertia..."
cargo build --release

echo "Installing binary to $BIN_PATH..."
run_root install -d -m 0755 "$BIN_DIR"
run_root install -m 0755 "$PROJECT_DIR/target/release/rinertia" "$BIN_PATH"

echo "Installing config under $CONFIG_DIR..."
run_root install -d -m 0755 "$CONFIG_DIR"
if [ ! -f "$CONFIG_PATH" ]; then
    run_root install -m 0644 "$PROJECT_DIR/dist/config.toml.example" "$CONFIG_PATH"
else
    echo "Keeping existing config: $CONFIG_PATH"
fi

echo "Installing udev rule..."
run_root install -m 0644 "$PROJECT_DIR/dist/99-rinertia.rules" "$UDEV_RULE"
if command -v udevadm >/dev/null 2>&1; then
    run_root udevadm control --reload-rules || true
    run_root udevadm trigger || true
fi

STYLE=$(detect_init_style)
case "$STYLE" in
    systemd)
        install_systemd_service
        ;;
    sysv)
        install_sysv_service
        ;;
    both)
        install_systemd_service
        install_sysv_service
        ;;
    none)
        echo "No supported init system detected; installed binary/config only."
        ;;
    *)
        echo "Unsupported INIT_STYLE='$STYLE' (use auto, systemd, sysv, both, or none)" >&2
        exit 2
        ;;
esac

echo "Installed rinertia."
echo "Config: $CONFIG_PATH"
echo "Binary: $BIN_PATH"
