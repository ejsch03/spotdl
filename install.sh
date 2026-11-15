#!/bin/sh
# Installs spotdl and its desktop app with a launcher and icon, for the current user.
#
#   ./install.sh               build, then install (the commands into ~/.local/bin; on
#                              Linux an app menu entry, on macOS ~/Applications/spotdl.app)
#   ./install.sh --uninstall   remove it again (the login and settings are kept)
#   PREFIX=/usr/local sudo -E ./install.sh   for everyone (build as yourself first)
set -eu

prefix="${PREFIX:-$HOME/.local}"
bin="$prefix/bin"
case "$(uname -s)" in
    Darwin) system=macos ;;
    *) system=linux ;;
esac

# Linux: menu entry and icon theme
apps="$prefix/share/applications"
icons="$prefix/share/icons/hicolor"
sizes="16 22 24 32 48 64 128 256 512"

# macOS: the app bundle, in ~/Applications (or /Applications when installing for everyone)
if [ "$(id -u)" -eq 0 ]; then
    app="${APPDIR:-/Applications}/spotdl.app"
else
    app="${APPDIR:-$HOME/Applications}/spotdl.app"
fi

# copies a file into place with the given mode, creating folders first (macOS's
# `install` has no -D)
put() {
    mkdir -p "$(dirname "$3")"
    install -m "$1" "$2" "$3"
}

refresh() {
    if [ "$system" = macos ]; then
        # Launchpad, Spotlight and the Dock pick up the app (and its icon) right away
        lsregister=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
        if [ -d "$app" ] && [ -x "$lsregister" ]; then
            "$lsregister" -f "$app" || true
        fi
        return
    fi
    # menus and icon caches pick up the change (each one only if it's there)
    command -v update-desktop-database >/dev/null && update-desktop-database -q "$apps" || true
    touch "$icons" 2>/dev/null || true # KDE checks this to drop its cached icons
    command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t -f "$icons" || true
    command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 >/dev/null 2>&1 || true
}

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$bin/spotdl" "$bin/spotdl-gui"
    if [ "$system" = macos ]; then
        rm -rf "$app"
    else
        rm -f "$apps/spotdl.desktop" "$icons/scalable/apps/spotdl.svg"
        for size in $sizes; do rm -f "$icons/${size}x${size}/apps/spotdl.png"; done
        refresh
    fi
    echo "Removed spotdl."
    exit 0
fi

cd "$(dirname "$0")"
if [ "$(id -u)" -eq 0 ]; then
    # building as root would leave root-owned files in target/
    if [ ! -x target/release/spotdl-gui ]; then
        echo "Build it as yourself first: cargo build --release --locked" >&2
        exit 1
    fi
else
    cargo build --release --locked
fi

put 755 target/release/spotdl "$bin/spotdl"
put 755 target/release/spotdl-gui "$bin/spotdl-gui"

if [ "$system" = macos ]; then
    version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
    rm -rf "$app"
    put 755 target/release/spotdl-gui "$app/Contents/MacOS/spotdl-gui"
    put 644 assets/icon/spotdl.icns "$app/Contents/Resources/spotdl.icns"
    mkdir -p "$app/Contents"
    sed "s|@VERSION@|$version|g" assets/macos/Info.plist > "$app/Contents/Info.plist"
    printf 'APPL????' > "$app/Contents/PkgInfo"
    # A signature covering the whole bundle gives macOS a stable identity for the app,
    # e.g. for remembering that it may write to an external drive.
    if ! codesign --force --sign - "$app" 2>/dev/null; then
        echo "Note: couldn't sign $app (codesign failed); it should still open." >&2
    fi
    refresh
    echo "Installed spotdl: open it from Launchpad or Spotlight, or run spotdl-gui."
    echo "The app is $app; the commands are in $bin."
else
    put 644 assets/icon/spotdl.svg "$icons/scalable/apps/spotdl.svg"
    for size in $sizes; do
        put 644 "assets/icon/png/spotdl-$size.png" "$icons/${size}x${size}/apps/spotdl.png"
    done
    mkdir -p "$apps"
    sed "s|@BINDIR@|$bin|" assets/spotdl.desktop > "$apps/spotdl.desktop"
    chmod 644 "$apps/spotdl.desktop"
    refresh
    echo "Installed spotdl into $prefix: find it in your app menu, or run spotdl-gui."
fi

case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "Note: $bin isn't on your PATH, so run the commands by their full path (the app works anyway)." ;;
esac
