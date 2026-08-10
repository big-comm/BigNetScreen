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
  'gettext'
)
optdepends=(
  'networkmanager: Miracast discovery and connection (Wi-Fi Direct)'
  'firewalld: automatically opens RTSP port 7236 on the P2P link'
  'intel-media-driver: encode por hardware VA-API em GPUs Intel'
  'libva-mesa-driver: encode por hardware VA-API em GPUs AMD'
  'xdg-desktop-portal-gnome: captura de tela no GNOME'
  'xdg-desktop-portal-kde: captura de tela no KDE'
)
makedepends=('rust' 'cargo' 'git')

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
  make PREFIX=/usr
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
