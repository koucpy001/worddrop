# my-croc

Cross-platform WAN file transfer: Windows GUI, macOS GUI, Linux CLI, Android.

Pair with a short word-code phrase (`nameplate-word-word-word`), transfer files
end-to-end encrypted with resumable progress. Self-host the rendezvous +
relay for stable transfers.

## Status

Workspace scaffolded. Under active development — see `.omo/plans/my-croc.md`
for the work plan and todos.

## Layout

- `crates/core` — pairing (SPAKE2 word-code), session state machine, iroh transfer engine, persistent identity, resume records
- `crates/rendezvous` — axum mailbox server (code <-> ticket, one-shot claim, TTL, rate limits)
- `crates/cli` — Linux CLI (send/receive by word code)

## License

MIT — see [LICENSE](LICENSE).
