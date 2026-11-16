## What and why

<!-- One or two sentences. Link the issue: Fixes #123 -->

## Checklist

- [ ] Every commit is signed off (`git commit -s`, see CONTRIBUTING.md)
- [ ] `cargo fmt -p siple-media -p siple-config -p siple-core -p siple-sip -p siple -p siple-cli` (not `--all`)
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace` (add `-- --skip visual_tests` off macOS)
- [ ] `cargo deny check` if dependencies changed
- [ ] New feature: JSON-RPC method, events and MCP tool included (API parity rule); `scripts/gen-api-docs.sh` run
- [ ] GUI change: snapshots regenerated on macOS (`UPDATE_SNAPSHOTS=1 cargo test -p siple visual`) and reviewed
- [ ] Docs / `docs/CONFIG.md` / `CHANGELOG.md` updated for user-visible changes
- [ ] No credentials, real hostnames or unredacted SIP traces in the diff

## Testing notes

<!-- How you verified it: OS, PBX, commands. -->
