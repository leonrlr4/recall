#!/usr/bin/env bash
# Builds recall from this checkout and installs it for the current user:
#
#   ~/.local/bin/recall                      the binary
#   ~/.local/lib/recall/                     sherpa-onnx and onnxruntime shared
#                                            libraries (the binary's rpath
#                                            includes $ORIGIN/../lib/recall)
#   ~/.config/systemd/user/recall.service    the user service
#
# then runs `recall models download` (about 1.7 GB, into the config's
# data_dir, ~/.local/share/recall/models by default).
#
# Usage: packaging/install.sh [--vulkan] [--dry-run]
#        packaging/install.sh --uninstall [--dry-run]
#
#   --vulkan     build whisper with the Vulkan backend (needs vulkan-headers
#                and shaderc)
#   --dry-run    print what would be done without doing it
#   --uninstall  remove the three paths above; recordings (~/Meetings) and
#                ~/.local/share/recall are never touched
set -euo pipefail

BIN_DIR="$HOME/.local/bin"
LIB_DIR="$HOME/.local/lib/recall"
UNIT_DIR="$HOME/.config/systemd/user"
UNIT="$UNIT_DIR/recall.service"

vulkan=0
dry_run=0
uninstall=0
for arg in "$@"; do
    case "$arg" in
        --vulkan) vulkan=1 ;;
        --dry-run) dry_run=1 ;;
        --uninstall) uninstall=1 ;;
        -h | --help)
            sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "install.sh: unknown option $arg (see --help)" >&2
            exit 2
            ;;
    esac
done

# Prints a command, and runs it unless this is a dry run.
run() {
    printf '+'
    printf ' %q' "$@"
    printf '\n'
    if [ "$dry_run" -eq 0 ]; then
        "$@"
    fi
}

has_systemctl() {
    command -v systemctl >/dev/null 2>&1
}

if [ "$uninstall" -eq 1 ]; then
    if [ -e "$UNIT" ] && has_systemctl; then
        run systemctl --user disable --now recall.service || true
    fi
    run rm -f "$UNIT" "$BIN_DIR/recall"
    run rm -rf "$LIB_DIR"
    if has_systemctl; then
        run systemctl --user daemon-reload
    fi
    echo
    echo "Removed recall. Recordings and ~/.local/share/recall (models, speakers, index) are kept."
    exit 0
fi

missing=()
for cmd in pipewire ffmpeg opencc cargo; do
    command -v "$cmd" >/dev/null 2>&1 || missing+=("$cmd")
done
if [ "${#missing[@]}" -gt 0 ]; then
    echo "install.sh: missing commands: ${missing[*]}" >&2
    echo "Install them with your package manager (cargo comes from rustup) and run this again." >&2
    exit 1
fi

cd "$(dirname "$0")/.."
target_dir="${CARGO_TARGET_DIR:-$PWD/target}"

build=(cargo build --release -p recall)
if [ "$vulkan" -eq 1 ]; then
    build+=(--features recall-pipeline/vulkan)
fi
run "${build[@]}"

# The directory holding the sherpa-onnx shared libraries this build linked:
# the copy beside the binary, else the prebuilt download, else a build
# script's output directory.
find_lib_dir() {
    local dir
    for dir in "$target_dir/release" "$target_dir"/sherpa-onnx-prebuilt/*shared*/lib "$target_dir"/release/build/*/out; do
        if [ -f "$dir/libsherpa-onnx-c-api.so" ]; then
            echo "$dir"
            return 0
        fi
    done
    return 1
}

run mkdir -p "$BIN_DIR" "$LIB_DIR" "$UNIT_DIR"
run install -m 755 "$target_dir/release/recall" "$BIN_DIR/recall"

if lib_src="$(find_lib_dir)"; then
    shopt -s nullglob
    libs=("$lib_src/libsherpa-onnx-c-api.so" "$lib_src"/libonnxruntime*.so*)
    shopt -u nullglob
    # -L copies the files symlinks point to, so the copies stand alone.
    run cp -L "${libs[@]}" "$LIB_DIR/"
elif [ "$dry_run" -eq 1 ]; then
    echo "+ cp -L <target>/.../libsherpa-onnx-c-api.so <target>/.../libonnxruntime*.so* $LIB_DIR/  (found after the build)"
else
    echo "install.sh: libsherpa-onnx-c-api.so not found under $target_dir after the build" >&2
    exit 1
fi

echo "+ sed 's|^ExecStart=.*|ExecStart=%h/.local/bin/recall daemon|' packaging/recall.service > $UNIT"
if [ "$dry_run" -eq 0 ]; then
    sed 's|^ExecStart=.*|ExecStart=%h/.local/bin/recall daemon|' packaging/recall.service >"$UNIT"
fi
if has_systemctl; then
    run systemctl --user daemon-reload
fi

run "$BIN_DIR/recall" models download

cat <<NEXT

recall is installed. Next steps:

  1. Make sure $BIN_DIR is on your PATH.
  2. Start the daemon now and at every login:
       systemctl --user enable --now recall
  3. Bind the hotkeys. For Hyprland, in ~/.config/hypr/hyprland.conf:
       bind = SUPER ALT, R, exec, recall start
       bind = SUPER ALT, S, exec, recall stop
       bind = SUPER ALT, D, exec, recall discard
  4. Open http://127.0.0.1:7474 for the archive.
NEXT
