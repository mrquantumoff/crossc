#!/usr/bin/env bash
# Installs crossc on Linux: the PipeWire SSC codec plugin and crossc-cli.
#
#   curl -fsSL https://raw.githubusercontent.com/mrquantumoff/crossc/master/linux/install.sh | bash
#
#   install.sh                      latest release (or, inside an extracted release, that release)
#   install.sh --version v0.4.0     that release
#   install.sh --local DIR          an extracted release in DIR (what `crossc-cli update` runs)
#   install.sh --from-source [DIR]  build the crossc checkout in DIR (default: the one holding this script)
#   install.sh --uninstall          also removes the UHQ auto service and its 96 kHz rule
#   --uhq auto|on|off               then run `crossc-cli uhq <mode>`: UHQ 96 kHz automatically for
#                                   audio above 48 kHz, always, or never (HiFi 48 kHz)
#   --no-restart                    leave PipeWire running; the plugin loads at its next start
#
# Root is needed only to write system directories; the script asks sudo for that step. CROSSC_REPO=owner/name
# picks another GitHub repository, PREFIX (default /usr/local) where crossc-cli goes, SPA_DIR PipeWire's
# SPA plugin directory (default: found automatically).

set -euo pipefail

REPO="${CROSSC_REPO:-mrquantumoff/crossc}"
PREFIX="${PREFIX:-/usr/local}"
MARKER="crossc-linux-plugin-version="

say() { printf 'crossc: %s\n' "$*"; }
warn() { printf 'crossc: warning: %s\n' "$*" >&2; }
die() { printf 'crossc: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "this needs '$1'; install it first"; }

# Runs a command that writes into directory $1, through sudo when that is not writable.
in_dir() {
    local dir="$1"
    shift
    while [ ! -d "$dir" ]; do dir="$(dirname "$dir")"; done
    if [ -w "$dir" ]; then
        "$@"
    else
        need sudo
        sudo "$@"
    fi
}

[[ "$REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || die "CROSSC_REPO=$REPO is not owner/name"

spa_dir() {
    local d
    if [ -n "${SPA_DIR:-}" ]; then
        [ -f "$SPA_DIR/bluez5/libspa-bluez5.so" ] || die "SPA_DIR=$SPA_DIR has no bluez5/libspa-bluez5.so"
        echo "$SPA_DIR"
        return
    fi
    if d="$(pkg-config --variable=plugindir libspa-0.2 2>/dev/null)" && [ -f "$d/bluez5/libspa-bluez5.so" ]; then
        echo "$d"
        return
    fi
    for d in /usr/lib/spa-0.2 /usr/lib64/spa-0.2 /usr/lib/x86_64-linux-gnu/spa-0.2 /usr/lib/aarch64-linux-gnu/spa-0.2; do
        if [ -f "$d/bluez5/libspa-bluez5.so" ]; then
            echo "$d"
            return
        fi
    done
    die "PipeWire's Bluetooth plugin (spa-0.2/bluez5/libspa-bluez5.so) is not installed"
}

is_ours() { [ -f "$1" ] && grep -aq "$MARKER" "$1"; }

# PipeWire with patches/pipewire-ssc.patch names the ssc slot in its codec loader; stock PipeWire
# gets the plugin in its (normally empty) LC3plus slot.
codec_slot() {
    local spa="$1"
    if grep -aq 'libspa-codec-bluez5-ssc' "$spa/bluez5/libspa-bluez5.so"; then
        echo ssc
        return
    fi
    local lc3plus="$spa/bluez5/libspa-codec-bluez5-lc3plus.so"
    if [ -e "$lc3plus" ] && ! is_ours "$lc3plus"; then
        die "$lc3plus is PipeWire's own LC3plus codec, so the SSC plugin cannot use that slot.
Build PipeWire with linux/patches/pipewire-ssc.patch, then run this again."
    fi
    echo lc3plus
}

pipewire_version() {
    pipewire --version 2>/dev/null | sed -n 's/.*Linked with libpipewire \([0-9][0-9.]*\).*/\1/p' | head -n1
}

# Runs a command in the desktop user's session (systemctl --user, PipeWire), also under sudo.
as_user() {
    if [ "$(id -u)" -ne 0 ]; then
        "$@"
    elif [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
        local uid
        uid="$(id -u "$SUDO_USER")"
        sudo -u "$SUDO_USER" XDG_RUNTIME_DIR="/run/user/$uid" \
            DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$uid/bus" "$@"
    else
        return 1
    fi
}

# Connected Bluetooth audio devices. Restarting PipeWire drops their A2DP link and
# Galaxy Buds do not bring it back by themselves.
connected_audio() {
    command -v bluetoothctl >/dev/null 2>&1 || return 0
    bluetoothctl devices Connected 2>/dev/null | while read -r _ mac _; do
        bluetoothctl info "$mac" 2>/dev/null | grep -q 'Icon: audio' && echo "$mac"
    done
}

reconnect() {
    local mac
    for mac in "$@"; do
        say "reconnecting $mac"
        bluetoothctl disconnect "$mac" >/dev/null 2>&1 || true
        sleep 2
        bluetoothctl connect "$mac" >/dev/null 2>&1 || warn "could not reconnect $mac; connect it again by hand"
    done
}

restart_pipewire() {
    [ "$NO_RESTART" = 1 ] && { say "restart PipeWire to apply this: systemctl --user restart pipewire wireplumber"; return; }
    local devices
    mapfile -t devices < <(connected_audio)
    if as_user systemctl --user restart pipewire wireplumber; then
        say "PipeWire restarted"
        sleep 2
        reconnect "${devices[@]}"
    else
        warn "could not restart PipeWire; run as your user: systemctl --user restart pipewire wireplumber"
    fi
}

UHQ_UNIT="crossc-uhq-auto.service"

user_config() {
    local home
    home="$(as_user sh -c 'echo "${XDG_CONFIG_HOME:-$HOME/.config}"' 2>/dev/null)" || return 1
    echo "$home"
}

# After installing: apply --uhq, or point an existing auto service at the new crossc-cli.
setup_uhq() {
    local cli="$PREFIX/bin/crossc-cli" cfg mode="$UHQ"
    cfg="$(user_config)" || { [ -n "$mode" ] && warn "no desktop user session; run: crossc-cli uhq $mode"; return 0; }
    if [ -z "$mode" ] && [ -f "$cfg/systemd/user/$UHQ_UNIT" ]; then
        mode=auto
    fi
    if [ -n "$mode" ]; then
        as_user "$cli" uhq "$mode" || warn "crossc-cli uhq $mode failed; run it again once the buds are connected"
    else
        say "UHQ 96 kHz: crossc-cli uhq auto (UHQ for audio above 48 kHz), uhq on, uhq off, uhq status"
    fi
}

remove_uhq() {
    local cfg
    cfg="$(user_config)" || return 0
    if [ -f "$cfg/systemd/user/$UHQ_UNIT" ]; then
        as_user systemctl --user disable --now "$UHQ_UNIT" >/dev/null 2>&1 || true
        as_user rm -f "$cfg/systemd/user/$UHQ_UNIT"
        as_user systemctl --user daemon-reload || true
        say "removed the UHQ auto service"
    fi
    if [ -f "$cfg/wireplumber/wireplumber.conf.d/51-crossc-uhq-rate.conf" ]; then
        as_user rm -f "$cfg/wireplumber/wireplumber.conf.d/51-crossc-uhq-rate.conf"
        say "removed the 96 kHz UHQ rule"
    fi
}

install_dir() {
    local dir="$1" spa slot version built_for running
    spa="$(spa_dir)"
    slot="$(codec_slot "$spa")"
    version="$(cat "$dir/VERSION")"
    [ -f "$dir/libspa-codec-bluez5-$slot.so" ] || die "$dir has no plugin for the $slot slot"
    [ -f "$dir/crossc-cli" ] || die "$dir has no crossc-cli"

    built_for="$(cat "$dir/PIPEWIRE_VERSION")"
    running="$(pipewire_version)"
    if [ -z "$running" ]; then
        warn "cannot tell which PipeWire is installed; the plugin is built for PipeWire $built_for"
    elif [ "$running" != "$built_for" ]; then
        warn "PipeWire is $running, the plugin is built for $built_for. PipeWire checks the codec ABI when it
loads the plugin and skips it if it does not match. If no SSC profile shows up, use --from-source."
    fi

    say "installing crossc $version: $spa/bluez5/libspa-codec-bluez5-$slot.so, $PREFIX/bin/crossc-cli"
    in_dir "$spa/bluez5" install -Dm755 "$dir/libspa-codec-bluez5-$slot.so" "$spa/bluez5/libspa-codec-bluez5-$slot.so"
    in_dir "$PREFIX/bin" install -Dm755 "$dir/crossc-cli" "$PREFIX/bin/crossc-cli"
    restart_pipewire
    setup_uhq
}

arch_name() {
    case "$(uname -m)" in
        x86_64 | amd64) echo x86_64 ;;
        *) die "no prebuilt release for $(uname -m); use --from-source" ;;
    esac
}

download_and_install() {
    local tag="$1" arch api ver asset tmp
    need curl
    need tar
    need sha256sum
    arch="$(arch_name)"
    if [ -z "$tag" ]; then
        api="https://api.github.com/repos/$REPO/releases/latest"
        tag="$(curl -fsSL -H 'Accept: application/vnd.github+json' "$api" |
            sed -n 's/^ *"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
        [ -n "$tag" ] || die "cannot find the latest release of $REPO"
    fi
    [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || die "'$tag' is not a release tag (vX.Y.Z)"
    ver="${tag#v}"
    asset="crossc-linux-$ver-$arch.tar.gz"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    say "downloading $asset from $REPO $tag"
    curl -fsSL -o "$tmp/$asset" "https://github.com/$REPO/releases/download/$tag/$asset" ||
        die "$tag has no $asset (Linux builds start with the first release that has one)"
    curl -fsSL -o "$tmp/$asset.sha256" "https://github.com/$REPO/releases/download/$tag/$asset.sha256"
    (cd "$tmp" && sha256sum -c --quiet "$asset.sha256") || die "checksum mismatch for $asset"
    tar -xzf "$tmp/$asset" -C "$tmp"
    install_dir "$tmp/crossc-linux-$ver-$arch"
}

build_and_install() {
    local src="$1" spa slot tmp
    [ -f "$src/linux/Makefile" ] && [ -f "$src/Cargo.toml" ] || die "$src is not a crossc checkout"
    need cargo
    need clang
    need pkg-config
    spa="$(spa_dir)"
    slot="$(codec_slot "$spa")"
    make -C "$src/linux" SLOT="$slot"
    cargo build --release --locked -p crossc-cli --manifest-path "$src/Cargo.toml"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    cp "$src/linux/target/release/libspa_codec_bluez5_ssc.so" "$tmp/libspa-codec-bluez5-$slot.so"
    cp "$src/target/release/crossc-cli" "$tmp/crossc-cli"
    sed -n 's/^version = "\(.*\)"/\1/p' "$src/linux/spa-codec-ssc/Cargo.toml" | head -n1 >"$tmp/VERSION"
    find "$src/linux/pipewire" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort -V | tail -n1 >"$tmp/PIPEWIRE_VERSION"
    install_dir "$tmp"
}

uninstall() {
    local spa f removed=0
    spa="$(spa_dir)"
    for f in "$spa/bluez5/libspa-codec-bluez5-ssc.so" "$spa/bluez5/libspa-codec-bluez5-lc3plus.so"; do
        if is_ours "$f"; then
            in_dir "$spa/bluez5" rm -f "$f"
            say "removed $f"
            removed=1
        fi
    done
    if [ -f "$PREFIX/bin/crossc-cli" ]; then
        in_dir "$PREFIX/bin" rm -f "$PREFIX/bin/crossc-cli"
        say "removed $PREFIX/bin/crossc-cli"
    fi
    [ "$removed" = 1 ] || say "no crossc plugin installed"
    remove_uhq
    restart_pipewire
}

ACTION=""
ARG=""
NO_RESTART=0
UHQ=
while [ $# -gt 0 ]; do
    case "$1" in
        --version) ACTION=download; ARG="${2:?--version needs a tag, e.g. v0.4.0}"; shift ;;
        --local) ACTION=local; ARG="${2:?--local needs a directory}"; shift ;;
        --from-source)
            ACTION=source
            if [ $# -gt 1 ] && [ "${2#--}" = "$2" ]; then ARG="$2"; shift; fi
            ;;
        --uninstall) ACTION=uninstall ;;
        --no-restart) NO_RESTART=1 ;;
        --uhq)
            UHQ="${2:?--uhq needs auto, on or off}"
            case "$UHQ" in auto | on | off) ;; *) die "--uhq takes auto, on or off" ;; esac
            shift
            ;;
        -h | --help) sed -n '2,18s/^# \{0,1\}//p' "${BASH_SOURCE[0]}" 2>/dev/null || true; exit 0 ;;
        *) die "unknown option $1 (see --help)" ;;
    esac
    shift
done

SELF_DIR=""
if [ -n "${BASH_SOURCE[0]:-}" ] && [ -f "${BASH_SOURCE[0]}" ]; then
    SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fi

case "$ACTION" in
    local) install_dir "$(cd "$ARG" && pwd)" ;;
    download) download_and_install "$ARG" ;;
    source) build_and_install "$(cd "${ARG:-${SELF_DIR:?run from a checkout or give its path}/..}" && pwd)" ;;
    uninstall) uninstall ;;
    "")
        if [ -n "$SELF_DIR" ] && [ -f "$SELF_DIR/VERSION" ] && [ -f "$SELF_DIR/crossc-cli" ]; then
            install_dir "$SELF_DIR"
        else
            download_and_install ""
        fi
        ;;
esac
