#!/usr/bin/env bash
# Build the external AUR recipe as the user; elevate only pacman transactions.
set -euo pipefail
if (( EUID == 0 )); then
    echo 'Do not run the NDI build as root.' >&2
    exit 1
fi

bns_install_dir=$(mktemp -d /tmp/bignetscreen-ndi.XXXXXXXX)
trap 'rm -rf -- "$bns_install_dir"' EXIT

/usr/bin/pkexec /usr/bin/pacman -S --needed --noconfirm base-devel git
/usr/bin/git -c core.hooksPath=/dev/null clone --depth 1 -- \
    https://aur.archlinux.org/ndi-sdk.git "$bns_install_dir/source"

# Keep downloads and build output outside the application checkout.
# Override user makepkg destinations and authentication only for this process.
mkdir -p "$bns_install_dir/config/pacman"
cat > "$bns_install_dir/config/pacman/makepkg.conf" <<'CONFIG'
PACMAN_AUTH=(/usr/bin/pkexec)
PKGDEST="$PWD"
SRCDEST="$PWD"
SRCPKGDEST="$PWD"
LOGDEST="$PWD"
BUILDDIR="$PWD"
CONFIG
cd "$bns_install_dir/source"
unset PKGDEST SRCDEST SRCPKGDEST LOGDEST BUILDDIR MAKEPKG_CONF
XDG_CONFIG_HOME="$bns_install_dir/config" /usr/bin/makepkg \
    --config /etc/makepkg.conf --syncdeps --install --noconfirm --nocolor
