# Host-controlled provider switching

This fork carries a compatibility patch for hosts that manage per-thread provider
selection on one App Server. Start the shared server with:

```sh
codex app-server --allow-host-provider-switching
```

The standalone `codex-app-server` executable accepts the same flag. It defaults
to false. Only the process startup flag (or an explicit host-owned
`AppServerRuntimeOptions` value) enables it. Request config, profiles, environment
variables and RPC clients cannot enable it. Embedded in-process servers retain
the default unless their host interface explicitly supports this option.

With the flag, saved and requested providers may differ from the server's current
provider selection. Resume still requires configured saved and requested provider
IDs. Input also requires the retained ID in the latest effective provider map,
including built-ins and host CLI definitions. A still-configured ID retains its
loaded definition until explicit close/resume; mutable local definition changes
do not silently reroute it. Every guarded input still loads current configuration
and managed policy; malformed configuration and managed provider selection or definition conflicts
still refuse execution. Authentication, network/destination policy, sandbox and
approval requirements remain authoritative. Built-in OpenAI routing and auth keep
their native semantics.

The host owns explicit routing consent and must fence input while closing and
resuming a thread on another provider. The flag does not change the route of an
already loaded thread. Use the existing public close/resume flow and original
thread ID; do not rewrite history, queues or goals. A blocked request leaves
history readable so configuration can be repaired.

Without the flag, the upstream provider-mismatch errors remain unchanged. This
exception deliberately changes the policy introduced by upstream
[PR #57840](https://github.com/openai/codex/pull/57840), which stops execution when
saved and selected providers differ.

## Carrying the patch to a stable release

Keep `main` as the upstream reference. Resolve the latest stable `rust-vVERSION`
release, fetch and record its peeled commit, and start
`workspace-candidate-rust-vVERSION-pN` on that exact commit. Replay only the
provider compatibility commit with its tests and this documentation. Inspect
upstream changes to `ConfigManager`, every common provider-guard caller, CLI
parsing and runtime/child startup. Compare old and new patch ranges with
`git range-diff`; account for every carried or retired change.

Run `just fmt-check`, inspect the complete release-base-to-head diff, and obtain
independent review of the committed candidate before expensive Rust builds.
After review, run the focused provider unit/protocol tests and CLI parsing tests
using the repository's `just test -p codex-app-server` and
`just test -p codex-cli` helpers with nextest filters. Verify actual inference
routing, same-ID switching, managed-policy refusal, malformed configuration,
missing providers and attempts to enable the exception through request config.
Then complete the host's native/state/child and package acceptance checks.

Publish candidates over SSH to `git@github.com:aither64/codex.git`. Only after
review and required checks pass, publish a retained
`workspace-release-rust-vVERSION-pN` branch and annotated `workspace-vVERSION-pN`
tag. Never rewrite those accepted refs. Advance the tested `workspace-patches`
branch with an exact force-with-lease when a rebase requires it; stop if the
remote tip differs from the recorded expected SHA. Do not merge the patch into
`main` or move upstream tags. A failed carry leaves the last accepted release
and moving branch intact.

Consumers pin the complete accepted source SHA and content hash. Record the
upstream base, downstream commit, recipe patches, Cargo dependency hash and exact
native binary hash separately: the CLI version alone does not identify this
build. Each release needs fresh review and verification. Retire the patch only
when tested upstream support meets both concurrent-provider and same-ID switching
requirements while preserving managed policy.
