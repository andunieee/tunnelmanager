# flipflop

Send files and text straight to your own devices.

Pair once with a trusted peer, a desktop or an Android phone, and flipflop
keeps a private channel open between you, at home or anywhere else. Drop files
or paste any text, and it lands directly on the other side. Each peer gets its
own history of what went back and forth.

No cloud, no accounts, and no internet required: peers find each other over
the local network or Bluetooth, and connect across the internet when they are
apart.

![flipflop on the desktop](docs/screenshot.png)

## Download

Get the latest Linux, macOS and Windows builds and the Android APK from the
[releases page](https://github.com/andunieee/flipflop/releases/latest).

## Build

```sh
cargo run --release          # desktop
just android-install         # Android (needs cargo-apk, Android SDK + NDK)
```

Architecture, platform notes and the Android toolchain setup are in
[docs/development.md](docs/development.md).
