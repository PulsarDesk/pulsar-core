<p align="center">
  <a href="https://pulsardesk.com"><img src=".github/assets/hero.png" alt="pulsar-core: the engine behind every Pulsar app" width="100%"></a>
</p>

<p align="center">The shared Rust engine behind Pulsar, the free remote desktop and game streaming app. 🧩</p>

<p align="center">
  <a href="https://github.com/PulsarDesk/pulsar-core/releases"><img src="https://img.shields.io/github/v/release/PulsarDesk/pulsar-core?include_prereleases&style=flat-square&color=5B57E0&label=version" alt="Version"></a>
  <a href="https://github.com/PulsarDesk/pulsar-core/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/PulsarDesk/pulsar-core/ci.yml?branch=main&style=flat-square&label=CI" alt="CI"></a>
  <a href="https://github.com/PulsarDesk/pulsar-core/stargazers"><img src="https://img.shields.io/github/stars/PulsarDesk/pulsar-core?style=flat-square&color=13B3D4" alt="Stars"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPLv3-5B57E0?style=flat-square" alt="License: GPLv3"></a>
</p>

<p align="center">
  <b>Looking for the app?</b> 👉 <a href="https://github.com/PulsarDesk/pulsar-desktop"><b>Pulsar Desktop</b></a> (Windows, macOS, Linux) · <a href="https://github.com/PulsarDesk/pulsar-mobile"><b>Pulsar Mobile</b></a> (Android)
</p>

---

## 🚀 Project Status
**Current Status:** Alpha (0.x). This is a Rust library, not an app. **Looking for Pulsar itself?** Get the
[desktop app](https://github.com/PulsarDesk/pulsar-desktop) or the [Android app](https://github.com/PulsarDesk/pulsar-mobile).

It handles what both apps share: device IDs, connecting (peer-to-peer first, relay as fallback), end-to-end
encryption, the streaming pipeline, controllers and LAN discovery.

---

## 📦 Other Pulsar Repositories
- [**pulsar-desktop**](https://github.com/PulsarDesk/pulsar-desktop) - Pulsar for Windows, macOS and Linux
- [**pulsar-mobile**](https://github.com/PulsarDesk/pulsar-mobile) - Pulsar for Android
- [**relay**](https://github.com/PulsarDesk/relay) - Run your own relay server
- [**pulsar-proto**](https://github.com/PulsarDesk/pulsar-proto) - Messages between the apps and the relay

---

## 🐞 Reporting a bug
Found a problem while using Pulsar? Report it in the app's repository
([desktop](https://github.com/PulsarDesk/pulsar-desktop/issues/new/choose) or
[mobile](https://github.com/PulsarDesk/pulsar-mobile/issues/new/choose)), even if you think the cause is here.
Open an issue in this repository only for problems with the library itself.

---

## 🤝 Contributing
Everyone can open [issues](https://github.com/PulsarDesk/pulsar-core/issues) and pull requests. Irrelevant or
abusive issues may be closed or removed. Commits follow [Conventional Commits](https://www.conventionalcommits.org).

### Development Guide
```bash
git clone https://github.com/PulsarDesk/pulsar-core.git
cd pulsar-core
cargo test
```

Use it as a git dependency (the Windows virtual gamepad needs the patch line too):

```toml
[dependencies]
pulsar-core = { git = "https://github.com/PulsarDesk/pulsar-core" }

[patch.crates-io]
vigem-client = { git = "https://github.com/PulsarDesk/pulsar-core" }
```

Design notes: [docs/adaptive-streaming.md](docs/adaptive-streaming.md).

---

## 📄 License
Pulsar is licensed under **GNU GPLv3 or later** (GPL-3.0-or-later). See the [LICENSE](LICENSE) file for details.
