# Status — what an agent picking up the repo should do next

Part of the pulsar-core agent docs — index: [`CLAUDE.md`](../../CLAUDE.md).

## Adaptive streaming rework — shipped, awaiting the maintainer's real-session test

**Design, phased plan and implementation status: [`../adaptive-streaming.md`](../adaptive-streaming.md).**
Read it end to end before touching the streaming path. Summary:

- Mid-stream the picture must **never freeze**; the client measures continuously and
  adapts automatically, on every encoder/decoder/transport, to the minimum bandwidth
  the path allows with the cleanest possible picture at that budget.
- Before the rework: two divergent bitrate-only controllers (desktop `play/hold.rs`, mobile
  `client.rs`), no resolution/fps ladder, no intra-refresh/FEC/LTR, 2 s GOP in remote
  mode, NACK useless when RTT > 100 ms → each loss was a multi-second freeze.
- **Phases 0–4 shipped 2026-09-03** (core v0.6.0 → desktop v0.11.0 / mobile v0.12.0),
  then a second pass on the maintainer's decisions (core v0.7.0 → desktop v0.12.0 / mobile
  v0.13.0): in-process libav host (Linux), in-process GStreamer for Wayland, Reed-Solomon
  FEC, resolution fixed / bitrate-only adaptation, NVENC intra refresh (Windows, unbuilt
  here), mobile host FEC + recovery. The controller is `src/adapt/` (pure, unit + scenario
  tested: `cargo test --test adapt_scenarios`).
- **None of it has been tested on a real session yet.** The next step is the maintainer's
  test (procedure in the design doc's "Implementation status"), then fixes from it.
