# pulsar-core

The shared **Rust core** of [Pulsar](https://github.com/PulsarDesk) — the free,
open-source remote-desktop + game-streaming app. Both the desktop and mobile apps
depend on this crate (as a git dependency).

It owns the performant, headless logic:

- **`Node`** — register with the relay → get a stable 9-digit device ID → try P2P
  hole-punch → fall back to relaying. Returns an encrypted `Session`.
- **Crypto** — X25519 + ChaCha20-Poly1305 end-to-end encryption (zero-knowledge).
- **Config** — user-editable relay + network mode (`auto` / `p2p-only` / `relay-only`).
- **Input** — controller detection/normalization (DS3/4/5/Xbox/standard) + a virtual
  gamepad backend (uinput on Linux, ViGEmBus on Windows via the vendored `vigem-client`).
- **Discovery** — LAN multicast peer discovery.
- **Service protocol + streaming pipeline** over the encrypted session.

Consumed by:

- [`PulsarDesk/pulsar-desktop`](https://github.com/PulsarDesk/pulsar-desktop)
- [`PulsarDesk/pulsar-mobile`](https://github.com/PulsarDesk/pulsar-mobile)

```toml
[dependencies]
pulsar-core = { git = "https://github.com/PulsarDesk/pulsar-core" }

# pulsar-core uses the vendored vigem-client fork on Windows — consumers must patch it:
[patch.crates-io]
vigem-client = { git = "https://github.com/PulsarDesk/pulsar-core" }
```

## Build & test

```bash
cargo test          # headless suite
```

(Controller *reading* lives in the apps via SDL3 — gilrs was removed from this
crate; here are the types/normalization + virtual-pad backends.)

Depends on [`pulsar-proto`](https://github.com/PulsarDesk/pulsar-proto) and
[`relay`](https://github.com/PulsarDesk/relay) (git dependencies).

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
