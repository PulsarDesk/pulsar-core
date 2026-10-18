# pulsar-core — shared Rust engine

The headless engine of Pulsar, consumed by `pulsar-desktop` and `pulsar-mobile`
as a **git dependency** (never a path dep). See the umbrella `../CLAUDE.md` for the
product; `README.md` here for the consumer snippet.

This file is the **index**. Read the topic file before working in its area:

| Topic | File | Read before touching… |
| --- | --- | --- |
| Workspace + modules | [docs/agents/modules.md](docs/agents/modules.md) | any module (`adapt`, `connection`, `crypto`, `config`, `input`, `pipeline`, `capture`, `audio`, `discovery`, `service`) or the vendored `vigem-client` |
| Status / next task | [docs/agents/status.md](docs/agents/status.md) | the streaming path — adaptive streaming shipped, awaiting a real test |
| Adaptive streaming design | [docs/adaptive-streaming.md](docs/adaptive-streaming.md) | `src/adapt/`, FEC, loss recovery, rate control |

## Must-know rules

- **Git dependency of both apps:** push here first, then `cargo update -p pulsar-core` in
  `pulsar-desktop` and `pulsar-mobile` and push them.
- Session messages (`service/wire.rs`) are JSON — additive changes are safe with
  `#[serde(default)]`. Relay messages (`pulsar-proto`) are bincode — positional, so any
  change is a protocol-version bump across proto/relay/core/apps + relay redeploy.
- **No real video encoder/decoder here** — those live in the apps (`pulsar-capture`,
  `pulsar-render`, mobile's Kotlin plugin). Here: traits, arg builders, Linux Wayland capture.
- **Never push a behaviour change before the maintainer tested it** on a real session.
  Docs and tests may be pushed.
- Rust via `rustup run stable cargo …` (distro cargo is too old). **Linux builds need the
  GStreamer dev headers** (`libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev`).
- UI copy is Turkish; the maintainer writes Turkish. Commit messages in English.

## Test

```bash
cargo test    # e2e (relay + 2 nodes), streaming, auth races, side channels,
              # disconnect, input service, adaptive scenarios, … (tests/ has 10 files)
```

Keep it green — it runs a real relay + two nodes, so it is the regression net for every
consumer.
