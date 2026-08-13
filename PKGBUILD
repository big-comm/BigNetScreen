# Maintainer: Tales A. Mendonça <talesam@gmail.com>
# Contributor: BigCommunity <https://github.com/big-comm>

pkgname=bignetscreen
pkgver=0.1.0
pkgrel=1
pkgdesc="Mirror your screen to Miracast (Wi-Fi Display) and Chromecast receivers"
arch=('x86_64' 'aarch64')
url="https://github.com/big-comm/BigNetScreen"
license=('GPL-3.0-or-later')

depends=(
  'gtk4'
  'libadwaita'
  'gstreamer'
  'gst-plugins-base'
  'gst-plugins-good'
  'gst-plugins-bad'      # mpegtsmux, openh264, va
  'gst-plugins-ugly'     # x264enc
  'gst-libav'            # avenc_aac
  'gst-plugin-pipewire'  # pipewiresrc
  'pipewire'
  'xdg-desktop-portal'
  # `pactl`: the application puts the default audio device back after a
  # session. Not optional — sharing a screen disturbs the sound settings on
  # desktops with an effects chain, and leaving that unrepaired is a fault the
  # person has to fix by hand.
  'libpulse'
)
optdepends=(
  'networkmanager: Miracast discovery and connection (Wi-Fi Direct)'
  'firewalld: automatically opens RTSP port 7236 on the P2P link'
  'intel-media-driver: VA-API hardware encoding on Intel GPUs'
  'libva-mesa-driver: VA-API hardware encoding on AMD GPUs'
  'xdg-desktop-portal-gnome: screen capture on GNOME'
  'xdg-desktop-portal-kde: screen capture on KDE'
)
# `gettext` is a build-time tool (msgfmt compiles the catalogues); at runtime
# the gettext functions come from glibc.
makedepends=('rust' 'cargo' 'git' 'gettext')

source=("$pkgname::git+$url.git")
sha256sums=('SKIP')

prepare() {
  cd "$pkgname"
  export RUSTUP_TOOLCHAIN=stable
  cargo fetch --locked
}

build() {
  cd "$pkgname"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_TARGET_DIR=target
  # What the application reports in its log and its About dialog. Taken from
  # the package's own version so the two can never disagree.
  export APP_VERSION="$pkgver-$pkgrel"
  make PREFIX=/usr APP_VERSION="$APP_VERSION"
}

check() {
  cd "$pkgname"
  export RUSTUP_TOOLCHAIN=stable
  cargo test --workspace --locked
}

package() {
  cd "$pkgname"
  make DESTDIR="$pkgdir" PREFIX=/usr install
  install -Dm644 COPYING "$pkgdir/usr/share/licenses/$pkgname/COPYING"
  install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
}
