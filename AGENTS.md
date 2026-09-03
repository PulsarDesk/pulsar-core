# Agent notes — pulsar-core

Read `CLAUDE.md` first (layout, modules, test command). This file holds what an agent
picking up the repo should do **next**, and the rules that are not derivable from code.

## Next task: adaptive streaming rework

**Design and phased plan: [`docs/adaptive-streaming.md`](docs/adaptive-streaming.md).**
Read it end to end before touching code. Summary:

- Mid-stream the picture must **never freeze**; the client measures continuously and
  adapts automatically, on every encoder/decoder/transport, to the minimum bandwidth
  the path allows with the cleanest possible picture at that budget.
- Today: two divergent bitrate-only controllers (desktop `play/hold.rs`, mobile
  `client.rs`), no resolution/fps ladder, no intra-refresh/FEC/LTR, 2 s GOP in remote
  mode, NACK useless when RTT > 100 ms → each loss is a multi-second freeze.
- **Phases 0–4 are implemented locally (2026-09-03) and await the maintainer's
  real-session test** — status, decisions, limits and the test procedure are in the design
  doc ("Implementation status"). Not pushed; desktop and mobile build against this core via
  a local `.cargo/config.toml` path patch (git-excluded; never commit the rewritten
  `Cargo.lock`). The controller is `src/adapt/` (pure, unit + scenario tested:
  `cargo test --test adapt_scenarios`).

## Rules

- This crate is a **git dependency** of `pulsar-desktop` and `pulsar-mobile`: push here
  first, then `cargo update -p pulsar-core` in each app and push them.
- Session messages (`service/wire.rs`) are JSON — additive changes are safe with
  `#[serde(default)]`. Relay messages (`pulsar-proto`) are bincode — positional, so any
  change is a protocol-version bump across proto/relay/core/apps + relay redeploy.
- **Never push a behaviour change before the maintainer tested it** on a real session.
  Docs and tests may be pushed.
- Rust via `rustup run stable cargo …` (distro cargo is too old). `cargo test` here runs
  the e2e suite (relay + two nodes) — keep it green.
- UI copy is Turkish; the maintainer writes Turkish. Commit messages in English.
