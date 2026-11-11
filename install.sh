#!/usr/bin/env bash
# PlayTogether installer for Linux. One time; afterwards use "PlayTogether" from your app menu
# (or run `playtogether`), and "Stop PlayTogether" / `playtogether-stop` to stop it.
#
#   From a clone:   ./install.sh
#   Without one:    curl -fsSL https://raw.githubusercontent.com/ashar-viraj/PlayTogether/main/install.sh | bash
#
# It downloads the prebuilt binary from the latest GitHub release. If there is none for this
# machine (or it doesn't run here), it builds from source, installing what that needs.
# On Windows, run install.bat instead.
set -euo pipefail

REPO="ashar-viraj/PlayTogether"
APP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/playtogether"
BIN_DIR="$HOME/.local/bin"
DESKTOP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
EXE="$APP_DIR/Playtogether"

say()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
  Linux) ;;
  MINGW*|MSYS*|CYGWIN*) die "this is Windows: run install.bat instead." ;;
  *) die "PlayTogether supports Linux (PipeWire) and Windows. This system is $(uname -s)." ;;
esac

# ---------------------------------------------------------------- PipeWire (needed at runtime)
if ! command -v pw-metadata >/dev/null 2>&1; then
  warn "PipeWire tools (pw-metadata) not found. PlayTogether needs PipeWire + WirePlumber"
  warn "(the default on Ubuntu 22.10+, Fedora 34+). Install them, then run PlayTogether."
elif ! pw-metadata -n default 0 >/dev/null 2>&1; then
  warn "PipeWire doesn't seem to be running right now; PlayTogether needs it when you start it."
fi

mkdir -p "$APP_DIR" "$BIN_DIR" "$DESKTOP_DIR"

# ---------------------------------------------------------------- 1. prebuilt binary
installed=0
case "$(uname -m)" in
  x86_64|amd64) asset="playtogether-linux-x86_64" ;;
  *) asset="" ;;
esac
if [ -n "$asset" ] && command -v curl >/dev/null 2>&1; then
  say "Downloading the latest PlayTogether release…"
  tmp="$(mktemp)"
  if curl -fsSL -o "$tmp" "https://github.com/$REPO/releases/latest/download/$asset"; then
    chmod +x "$tmp"
    if "$tmp" --version >/dev/null 2>&1; then
      mv "$tmp" "$EXE"
      installed=1
      say "Installed the prebuilt binary."
    else
      warn "The prebuilt binary doesn't run on this system (too old/new libraries?); building from source instead."
    fi
  else
    say "No prebuilt release available; building from source instead."
  fi
  rm -f "$tmp"
fi

# ---------------------------------------------------------------- 2. build from source
if [ "$installed" = 0 ]; then
  # Source: this clone, or a fresh shallow clone.
  here="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || true)"
  if [ -n "$here" ] && [ -f "$here/Cargo.toml" ] && grep -q 'name = "Playtogether"' "$here/Cargo.toml"; then
    SRC="$here"
  else
    command -v git >/dev/null 2>&1 || die "git is needed to fetch the source (e.g. sudo apt install git)."
    SRC="$(mktemp -d)/PlayTogether"
    say "Fetching the source…"
    git clone --depth 1 "https://github.com/$REPO.git" "$SRC"
  fi

  # Build dependencies: PipeWire headers, libclang (for bindgen), pkg-config, a C toolchain.
  have_deps() {
    pkg-config --exists libpipewire-0.3 2>/dev/null && command -v cc >/dev/null 2>&1 &&
      { ls /usr/lib*/libclang*.so* /usr/lib/*/libclang*.so* /usr/lib/llvm-*/lib/libclang*.so* >/dev/null 2>&1; }
  }
  if ! have_deps; then
    SUDO=""; [ "$(id -u)" -ne 0 ] && SUDO="sudo"
    say "Installing build dependencies (you may be asked for your password)…"
    if command -v apt-get >/dev/null 2>&1; then
      $SUDO apt-get update -qq && $SUDO apt-get install -y libpipewire-0.3-dev libclang-dev pkg-config build-essential curl \
        || { warn "apt install failed; trying the no-sudo header bootstrap"; bash "$SRC/scripts/bootstrap-deps.sh"; }
    elif command -v dnf >/dev/null 2>&1; then
      $SUDO dnf install -y pipewire-devel clang-devel pkgconf-pkg-config gcc curl
    elif command -v pacman >/dev/null 2>&1; then
      $SUDO pacman -S --needed --noconfirm pipewire clang pkgconf base-devel curl
    elif command -v zypper >/dev/null 2>&1; then
      $SUDO zypper install -y pipewire-devel clang-devel pkg-config gcc curl
    else
      die "unknown package manager: install the PipeWire development headers, libclang and pkg-config, then re-run."
    fi
  fi

  # Rust.
  if ! command -v cargo >/dev/null 2>&1 && [ -x "$HOME/.cargo/bin/cargo" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    say "Installing Rust (rustup, for your user only)…"
    curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
    export PATH="$HOME/.cargo/bin:$PATH"
  fi

  say "Building PlayTogether (a few minutes the first time)…"
  ( cd "$SRC" && cargo build --release )
  install -m 755 "$SRC/target/release/Playtogether" "$EXE"
  say "Built and installed."
fi

# ---------------------------------------------------------------- 3. launchers
cat > "$BIN_DIR/playtogether" <<EOF
#!/usr/bin/env bash
# Starts PlayTogether and opens its dashboard. Ctrl+C (or playtogether-stop) stops it.
( sleep 2; xdg-open http://localhost:8090/host >/dev/null 2>&1 || true ) &
exec "$EXE" "\$@"
EOF
cat > "$BIN_DIR/playtogether-stop" <<'EOF'
#!/usr/bin/env bash
# Stops PlayTogether cleanly (it restores your normal audio output on the way out).
if pkill -INT -x Playtogether; then echo "PlayTogether stopped."; else echo "PlayTogether wasn't running."; fi
EOF
chmod +x "$BIN_DIR/playtogether" "$BIN_DIR/playtogether-stop"

cat > "$DESKTOP_DIR/playtogether.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=PlayTogether
Comment=Play this PC's audio on every phone and PC in the room, in sync
Exec=$BIN_DIR/playtogether
Icon=audio-speakers
Terminal=true
Categories=AudioVideo;Audio;
EOF
cat > "$DESKTOP_DIR/playtogether-stop.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Stop PlayTogether
Comment=Stop PlayTogether and restore your normal audio output
Exec=$BIN_DIR/playtogether-stop
Icon=media-playback-stop
Terminal=false
Categories=AudioVideo;Audio;
EOF
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$DESKTOP_DIR" >/dev/null 2>&1 || true

case ":$PATH:" in *":$BIN_DIR:"*) ;; *) warn "$BIN_DIR isn't on your PATH; use the app menu, or add it to PATH." ;; esac

say "Done! Start it from your app menu (\"PlayTogether\") or run: playtogether"
say "Stop it with \"Stop PlayTogether\", Ctrl+C in its window, or: playtogether-stop"
