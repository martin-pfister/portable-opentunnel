# Upgrading the OpenTunnel SDK

[`Cargo.toml`](../Cargo.toml) pins `opentunnel = "=0.6.2"`. The audited
[upstream revision](https://github.com/anomalyco/opentunnel/tree/1ad859e17924ec3f6741cbc4cdb54ce381c9007c/crates/opentunnel)
is `1ad859e17924ec3f6741cbc4cdb54ce381c9007c`, recorded in the published crate's
`.cargo_vcs_info.json`. The [full 0.6.2 audit and reproduction commands](https://github.com/martin-pfister/portable-opentunnel/blob/9e1f4a32b1b69e4eb3adb4196807b5a487f0aa52/SDK-AUDIT.md)
remain in Git history. Treat those findings as evidence about that revision,
not a guarantee for newer releases.

## Contracts the wrapper relies on

- **Credential writers must stop before deletion.** The SDK's certificate
  refresh task has no joined handle in 0.6.2. `Tunnel::close()` joins the
  supervisor, not that writer. Credential writes are synchronous within SDK
  task polls. The wrapper uses a current-thread runtime and tears it down
  before remote deletion on a fresh runtime and then local cleanup.
  `shutdown_background()` is not a general join: blocking work such as DNS
  can outlive it and must not be able to write credentials.
- **Provisioning can be interrupted safely only after credentials are saved.**
  `CreatingTunnel` precedes the create request; `RequestingCertificate`
  follows the pending credential write. The wrapper keeps polling an
  interrupted create within its shutdown budget to reach that point.
  A registration whose response never arrives may still be impossible to
  delete locally.
- **Deletion is remote-first and non-recursive.** `Client::remove` skips the
  remote request for pending-only identities, and SDK profile removal is
  recursive. Use the saved complete or pending `id` and token with
  `api().remove()`. Treat success and confirmed 404 as settled, not timeout,
  transport failure, or rejected credentials. Persist the wrapper's deletion
  marker before removing recognized local files; preserve unrelated entries.
- **State is private and locked by the wrapper.** Check the storage root,
  file modes, atomic writes, and temporary names against
  [`src/state/layout.rs`](../src/state/layout.rs). The wrapper relies on `tunnel.json`
  being committed last and on older state remaining readable, including
  pending credentials. The SDK does not lock the profile.
- **Observe status without taking the task handle.** `Connected` means
  attached; `Stopped` is terminal. Read the watch receiver's current value
  before waiting for `changed()` so an already-reached state is not missed.
  Do not race `Tunnel::wait()` against signals: it takes the task handle.
- **Keep control-plane trust explicit.** The wrapper pre-validates extra CA
  files because the SDK can log and ignore bad input. Check TLS verification,
  API and bridge proxy behavior, and environment variable precedence. The API
  base must remain an origin; the SDK joins absolute paths against it.

## Upgrade checklist

1. **Identify the published source.** Record the new crate version and revision
   from `.cargo_vcs_info.json`; do not rely on a movable tag. Diff the old and
   new upstream `crates/opentunnel` trees and read the changed storage, runtime,
   protocol, and transport paths, including newly called helpers.
2. **Trace every credential writer.** Check all callers of storage methods,
   task spawning and joins, and whether any `tokio::fs`, blocking pool, or
   thread can carry writes past teardown. Recheck file formats, permissions,
   temporary names, and commit order. If the runtime or state contract
   changes, adapt and test the wrapper before merging the pin bump.
3. **Check API behavior.** Verify the provisioning callback order, complete and
   pending identity loading, removal retry/error semantics, status transitions,
   route and target validation, and CA/proxy behavior. Check resolved features
   (especially Tokio and TLS) and any new dependency in the write path.
4. **Run the gates.** Update the pin and lockfile together. Run
   `scripts/ci/local-checks.sh`, which runs the same
   [tests as CI](ci.md#how-the-tests-run).
   Pay particular attention to the delayed refresh, interrupted create,
   pending-only delete, unrelated-file preservation, and TLS fixture cases in
   [`tests/lifecycle.rs`](../tests/lifecycle.rs).
5. **Leave a record.** Update this guide with the new version and exact
   revision, any changed constraints, and a link to the review or test results.
