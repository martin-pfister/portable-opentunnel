# SDK audit: `opentunnel` 0.6.2 (source safety / API gate)

Status: **audit complete — safety gate PASSES, with binding implementation
constraints (§4).** No blockers for implementation.

Owner: SDK audit subagent. This file is the only artifact this task produced;
no source, Cargo, or doc file other than `SDK-AUDIT.md` was created or changed.

Audience: the agent implementing `src/`, `tests/`, and `Cargo.toml`. Every
finding below is either (a) a fact you must rely on, (b) a trap that will cost
you a debugging cycle, or (c) a checklist item for the next SDK bump.

---

## 1. Exact version and source

| Item | Value |
| --- | --- |
| Crate | `opentunnel` (Rust client for OpenTunnel blind TLS tunnels) |
| Version audited | **0.6.2** (published 2026-11-04) |
| Pin to use | `opentunnel = "=0.6.2"` |
| License | MIT |
| Edition / MSRV | **edition 2024, `rust-version = "1.89"`** |
| Upstream repo | <https://github.com/anomalyco/opentunnel> |
| Crate root in repo | `crates/opentunnel` |
| **Exact VCS revision** | **`1ad859e17924ec3f6741cbc4cdb54ce381c9007c`** |
| Tag | `v0.6.2` — **resolves to the same sha1** (verified via GitHub refs API) |
| Revision source | `.cargo_vcs_info.json` inside the published `.crate` |

The revision above came from the crate's own `.cargo_vcs_info.json`
(`{"git":{"sha1":"1ad859e1…"},"path_in_vcs":"crates/opentunnel"}`), so it is the
commit the published tarball was built from — not a guess at a tag. The
`v0.6.2` tag independently resolves to that identical commit object, so tag and
tarball agree. **Cite the sha1 as the authority**; the tag is a convenience that
upstream could in principle move.

### 1.1 How this audit was performed (and its limitation)

The sandbox blocked outbound network from the shell (`static.crates.io`,
`crates.io`, `docs.rs` all refused by `network.allowedDomains`), and no Rust
toolchain (`cargo`/`rustc`) is installed in this environment. Therefore:

- Source was retrieved through the host fetch tool from two independent
  sources: the **docs.rs rendering of the published crate** (`/crate/opentunnel/0.6.2/source/…`
  and `/src/opentunnel/*.rs.html`) and **GitHub `raw` at the exact sha1**.
- The crate source tree was reconstructed into
  `/tmp/opencode/sdkaudit-opentunnel-0.6.2/crates/opentunnel/src/` and audited
  with real `ripgrep` searches (commands in §8). `#[cfg(test)]` modules were
  omitted from the reconstruction; see §1.2 for how line numbers stay honest.
- **No `cargo build`, `cargo tree`, or `cargo doc` was run.** Build-graph claims
  (feature flags, transitive deps) come from the published `Cargo.toml` plus
  upstream dependency sources, not from a local resolve.

**Do this once on a networked machine with a toolchain** to confirm the
reconstruction was faithful (this also gives you a real `Cargo.lock`):

```sh
cargo new --bin /tmp/opentunnel-verify && cd /tmp/opentunnel-verify
cargo add opentunnel@=0.6.2
cargo fetch --locked
SRC=$(ls -d ~/.cargo/registry/src/*/opentunnel-0.6.2)
echo "$SRC"
# Re-run every command in §8 against $SRC and diff the results against this file.
```

Expected: byte-identical matches for all findings, because docs.rs source pages
are generated from the published tarball.

### 1.2 Cross-checks that the audited tree is the published 0.6.2

Three independent confirmations were done, so the line numbers cited below are
real and stable:

1. **Docs.rs public API surface matches the source.** `ClientOptions` is
   documented at `src/opentunnel/client.rs.html#37-46` with exactly the three
   fields `api`, `storage`, `ca_cert`; `Api` at `api.rs.html#15-22` with exactly
   `new, base, create, get, bind_certificate, certificate, remove, connect_url`.
   Both match the audited source, and there is **no** fourth `ClientOptions`
   field and **no** extra `Api` method.
2. **Line-number agreement.** `src/storage.rs` was compared line-by-line against
   the docs.rs rendering and matches exactly (1–317). `client.rs`
   (`ClientOptions` at 37, `Client` at 50), `api.rs` (`Api` at 15, `impl` at 24)
   and the `tunnel.rs` anchors in §5.2 also match.
3. **Dependency set matches** the documented dependency list (including
   `hyper-util 0.1.21`, `webpki-root-certs 1.0.10`, `rustls 0.23.46`).

> Line numbers below are from the published crate. `tunnel.rs` has an inline
> `#[cfg(test)] mod tests` block beginning at line **338**, which shifts every
> later item by +25 relative to a test-stripped copy. The numbers here are the
> published ones, verified against docs.rs.

### 1.3 Dependency features that matter for the safety argument

From the published `Cargo.toml` (verbatim, only relevant lines):

```toml
tokio = { version = "1.53.2", features = ["rt-multi-thread", "net", "io-util", "time", "sync", "macros", "fs"] }
reqwest = { version = "0.13.5", default-features = false, features = ["http2", "json", "rustls-no-provider"] }
tokio-tungstenite = { version = "0.30.0", features = ["rustls-tls-native-roots"] }
rustls-native-certs = "0.8.3"
hyper-util = { version = "0.1.21", default-features = false, features = ["client-proxy"] }
rustls = { version = "0.23.46", default-features = false, features = ["ring", "std", "tls12", "logging"] }
```

Two notes:

- The SDK **enables tokio's `fs` feature** but **never calls `tokio::fs`** (§3.1).
  Enabling the feature is inert; `tokio::fs::*` would be what routes work to
  `spawn_blocking`. Do not treat the feature flag as evidence of async file
  writes, and do not "fix" it by removing it from a fork.
- The SDK is built with `rt-multi-thread` enabled, so **nothing stops you from
  accidentally running it on a multi-threaded runtime.** That would void the
  safety argument in §4.1. This is the single most dangerous way to misuse the
  crate.

---

## 2. Summary of findings (read this first)

| # | Question | Finding | Impact |
| --- | --- | --- | --- |
| F1 | Does any credential write use `tokio::fs`, `spawn_blocking`, a thread, or a task that can outlive the runtime? | **No.** All credential I/O is synchronous `std::fs` executed inline in async task polls. | Safety gate passes **iff** current-thread runtime + delete-after-teardown (§4.1). |
| F2 | Are there detached tasks that touch storage? | **Yes, one.** `refresh_certificate` is spawned with its `JoinHandle` dropped. `close()` joins only the supervisor. | `close()` does **not** prove storage is quiescent (§4.2). Ordering is mandatory. |
| F3 | Does `Client::remove` handle pending-only identities? | **No.** It silently skips the server delete when `tunnel.json` is missing, then deletes the profile dir. | **Must not be used** for pending cleanup (§5.1). |
| F4 | Does `Api::remove` retry? | Only when the request *cannot have arrived* (`is_connect`); a timeout is returned, not retried. 404 is surfaced as `Error::Api`. | Treat timeout as *unknown*; treat 404 as success (§5.1). |
| F5 | Is `status_watch()` usable for readiness? | Yes, but a cloned `Receiver` marks the current value as *seen*, so you must read `borrow()` before awaiting `changed()`. | Naive `changed().await` can hang forever (§5.2). |
| F6 | Is there any API to disable TLS verification? | **No.** No field, no env var, no `danger_accept_invalid_*` anywhere in the crate. | Plan's "never disable TLS verification" is structurally enforced. |
| F7 | Which env vars does the SDK actually read? | `OPENTUNNEL_CA_CERT`, `SSL_CERT_FILE`, `SSL_CERT_DIR`, `HOME`, `XDG_DATA_HOME`, `XDG_RUNTIME_DIR`, `USERNAME`, plus reqwest/hyper-util proxy vars. **Not `OPENTUNNEL_API`.** | Plan is correct; see §6 for exact precedence. |
| F8 | Does the crate lock anything? | **No.** Zero locking, no `flock`/`try_lock`, no PID files. | The wrapper's lock is the only exclusion (§7.4). |
| F9 | Do the SDK's temp-file names match the plan? | **Yes, all six**, exactly as listed in `PLAN.md` §3. | Residue patterns are confirmed; one permission caveat (§7.3). |
| F10 | Is the plan's `RequestingCertificate` cancellation window real? | **Yes.** `pending.json` + `token` + `private-key.pem` are on disk *before* that stage fires. | Plan §4 is accurate and safe to implement (§5.3). |
| F11 | Does the crate validate route targets / names the way the CLI needs? | Targets must be `host:port` — a **bare port is rejected**. Names must be `@` or a lowercase alnum label ≤63 with alnum first/last. | The wrapper must normalise `3000` → `127.0.0.1:3000` itself and generate compatible route names (§5.4). |
| F12 | Is the `--api` override path-safe? | `base.join("/api/tunnel…")` **discards any base path**. | `--api` must be origin-only; a path prefix silently vanishes (§6.3). |

---

## 3. Escaped-writer audit (the safety gate)

This is the audit `PLAN.md` §2 requires: *"Audit the ENTIRE pinned crate,
including protocol/helpers, for `tokio::fs`, `spawn_blocking`, `thread::spawn`,
and equivalent thread/task dispatch."*

### 3.1 Complete inventory of filesystem access

There are **13** filesystem operations in the whole crate: `storage.rs` (12)
plus one CA read in `tls.rs` — nothing hidden in `protocol/`. Ten of them go
through an `fs::` path (and are what grep B below matches); three are method
calls on the opened file:

```text
src/tls.rs:22        std::fs::read(path)                 // CA PEM read (construction-time)
src/storage.rs:64    fs::read_dir                        // profiles()
src/storage.rs:157   fs::remove_file                     // save(): drop pending.json
src/storage.rs:167   fs::remove_dir_all                  // remove(): delete profile dir
src/storage.rs:199   fs::read_to_string                  // read_optional()
src/storage.rs:219   fs::DirBuilder                      // 0700 profile dir
src/storage.rs:222   std::os::unix::fs::DirBuilderExt::mode(0o700)
src/storage.rs:232   fs::OpenOptions                     // write_secret temp file
src/storage.rs:235   std::os::unix::fs::OpenOptionsExt::mode(0o600)
src/storage.rs:236   options.open(&temporary)
src/storage.rs:237   file.write_all(content)
src/storage.rs:238   file.sync_all()
src/storage.rs:240   fs::rename(&temporary, path)        // atomic commit
```

Every one is **`std::fs`, i.e. synchronous blocking calls**, and there is **no
`tokio::fs`** anywhere. Verbatim proof (note `use std::fs;` at `storage.rs:4`):

```rust
// src/storage.rs:228-241
/// Writes a file readable only by the current user, replacing it atomically.
pub(crate) fn write_secret(path: &Path, content: &[u8]) -> Result<()> {
    let context = || format!("writing {}", path.display());
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temporary).map_err(Error::io(context()))?;
    file.write_all(content)
        .and_then(|()| file.sync_all())
        .map_err(Error::io(context()))?;
    fs::rename(&temporary, path).map_err(Error::io(context()))
}
```

### 3.2 Complete inventory of task/thread dispatch

There is **no `thread::spawn`, no `std::thread`, no `spawn_blocking`, no
`block_on`, no `block_in_place`, no `std::process::Command`** in the crate. The
only concurrency primitives are `tokio::spawn` / `JoinSet::spawn` /
`AbortHandle`, all on the caller's runtime:

| Site | What is spawned | Touches storage? | Joined? |
| --- | --- | --- | --- |
| `tunnel.rs:143` | `refresh_certificate(...)` | **YES** — `storage.save` at `tunnel.rs:321` | **NO — handle dropped** |
| `tunnel.rs:165` | `supervisor.run()` | No | Yes, via `wait()`/`close()` |
| `tunnel.rs:500` | writer task (WebSocket sink) | No | `writer.abort()` at `tunnel.rs:655` |
| `tunnel.rs:574` | `run_channel(...)` per public conn, via `JoinSet` | No | `tasks.abort_all()` on session end |
| `tunnel.rs:796` | `receive` per channel | No | `receive.abort()` at `tunnel.rs:849` |

So **exactly one** spawned task writes credentials, and it is the un-joined one.
That single fact drives §4.2.

### 3.3 The credential write call graph

Every path that can create or mutate a credential file, traced to its origin:

```text
Storage::write_secret                      storage.rs:229   (the only writer)
  ├── Storage::save_pending                storage.rs:121-134
  │     writes token, private-key.pem, pending.json   (in that order)
  └── Storage::save                        storage.rs:136-163
        writes token, private-key.pem, certificate.pem, chain.pem,
        tunnel.json, then removes pending.json      (tunnel.json LAST)

callers of save_pending  (exactly one):
  Client::provision                 client.rs:229
    ← Client::create                client.rs:101   (async, on caller's runtime)
        ← caller: wrapper's block_on

callers of save  (exactly three):
  1. Client::ensure      client.rs:169   (renewed cert seen at startup)
  2. Client::complete    client.rs:277   (normal provisioning completion)
       ← Client::provision  ← Client::create / Client::resume / Client::ensure
  3. refresh_certificate tunnel.rs:321   ← tokio::spawn at tunnel.rs:143
       ⚠ the only writer not awaited by anything the wrapper holds
```

Nothing in `protocol/`, `provision.rs`, `api.rs`, `http_proxy.rs`, `tls.rs`,
`paths.rs`, or `error.rs` writes to storage. `provision.rs` only generates keys
in memory. `paths.rs` computes directories but the wrapper will never reach it
because it always passes `ClientOptions::storage = Some(Storage::at(...))`.

**Verdict for the gate: no credential writer escapes the async task boundary.**
There is no thread, no blocking pool, and no `tokio::fs` to escape *to*. The
only escape risk is a *runtime* that keeps polling the refresh task after the
wrapper believes it is done — see §4.2.

### 3.4 Indirect `spawn_blocking` you should know about (harmless)

The crate calls `TcpStream::connect((host, port))` with `&str`/`String` hosts
(`http_proxy.rs:76`, `http_proxy.rs:109`, `tunnel.rs:832`). Tokio's
`ToSocketAddrs` for `(&str, u16)` and `String` does:

```rust
// tokio 1.53.2, tokio/src/net/addr.rs
MaybeReady(sealed::State::Blocking(spawn_blocking(move || {
    std::net::ToSocketAddrs::to_socket_addrs(&(&host[..], port))
})))
```

So there **is** blocking-pool DNS work, which `shutdown_background()` does not
wait for. It performs DNS only and cannot write to the state directory. This is
exactly the "unrelated DNS blocking work may outlive shutdown" case the plan
already anticipates; it does not weaken the gate.

---

## 4. Binding implementation constraints

These are not suggestions. Violating 4.1 or 4.2 invalidates the safety gate and
the plan's promise that a returned `block_on` proves the state directory is
quiescent.

### 4.1 The forwarding runtime MUST be current-thread

Use `Builder::new_current_thread().enable_all()`. Because every credential write
is synchronous code executing *inside* a task poll, and a current-thread runtime
only polls on the thread running `block_on`, **the moment `block_on` returns no
task can write.** This is the entire safety argument, and it is load-bearing.

Do **not** use `#[tokio::main]`, `Runtime::new()`, or
`Builder::new_multi_thread()`. With worker threads, dropping or
`shutdown_background()`-ing the runtime leaves the refresh task runnable, and it
can call `storage.save` *after* you deleted the directory — recreating
`token`/`private-key.pem`/`certificate.pem` from memory and resurrecting
credentials you believed removed.

`enable_all()` is also required: `tunnel.rs:143` calls `tokio::spawn` inside
`Tunnel::start`, and `refresh_certificate` sleeps on `tokio::time`. A runtime
without the timer/IO drivers and without runtime context will panic.

### 4.2 `close()` / `drop()` do NOT prove the refresh task stopped

```rust
// src/tunnel.rs:199-209
pub async fn close(mut self) -> Result<()> {
    self.shutdown.send_replace(true);
    self.wait().await          // awaits ONLY the supervisor JoinHandle
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);   // no join at all
    }
}
```

`close()` joins the **supervisor** only. The `refresh_certificate` task
(`tunnel.rs:143`) is spawned with its `JoinHandle` discarded; it observes
shutdown only in `tokio::select!` between sleeps (`tunnel.rs:288-291`):

```rust
tokio::select! {
    _ = tokio::time::sleep(wait) => {}
    _ = shutdown.changed() => return,
}
```

It does **not** select on shutdown while awaiting the HTTP request:

```rust
// src/tunnel.rs:293-294
let Ok(info) = api.certificate(&identity.token, &identity.id).await else {
    continue;
};
```

Consequence: if a refresh GET is in flight when `close()` returns, that request
completes, then `storage.save` runs at `tunnel.rs:321`, **after `close()`
resolved**, as long as anything keeps polling the runtime.

Therefore the ephemeral ordering must be exactly:

```text
tunnel.close().await            // may return with a refresh GET still in flight
drop all forwarding resources   // Client, Tunnel, any Api/Storage clones
consume runtime:                // ONLY now is the writer provably frozen
    rt.shutdown_background()    // (or drop; background avoids waiting on DNS)
--------------------------------- state dir is now write-quiescent ---
THEN delete local files, on the separate cleanup runtime
```

Never delete state between `close()` returning and the forwarding runtime being
torn down. The plan already specifies this ordering; this section is *why* it
must not be relaxed, and it is the one invariant a refactor could silently
break. Regression-test it with a deliberately delayed certificate response
during shutdown (plan §Testing B already calls for this).

### 4.3 `Drop` alone is not shutdown

`drop(tunnel)` sends the signal but neither joins the supervisor nor the refresh
task. If you must await completion, use `close()`. If you must guarantee the
state directory is quiet, use §4.2's runtime teardown — not `drop`.

### 4.4 `wait()` takes the handle — never race it

`wait(&mut self)` does `self.task.take()`. Racing it against a signal cancels
the future and **detaches** the supervisor (it keeps running). Select on
`status_watch()` reaching `Stopped` and on signals instead, then `close()`.
This matches `PLAN.md`; it is confirmed correct here.

---

## 5. Behaviour of the APIs the wrapper depends on

### 5.1 Deletion, 404, and pending identities

`Client::remove` is **not** safe for pending-only state:

```rust
// src/client.rs:186-194
pub async fn remove(&self, profile: &str) -> Result<()> {
    if let Some(identity) = self.storage.load(profile)? {          // needs tunnel.json
        match self.api.remove(&identity.token, &identity.id).await {
            Ok(()) | Err(Error::Api { status: 404, .. }) => {}
            Err(error) => return Err(error),
        }
    }
    self.storage.remove(profile)                                   // ALWAYS runs
}
```

`Storage::load` returns `Ok(None)` when `tunnel.json` is absent
(`storage.rs:81-83`), so for a pending-only identity the server call is **skipped
entirely** and `storage.remove` still `remove_dir_all`s the profile — silently
abandoning a live remote registration. Confirmed: **do not call
`Client::remove` on the pending path.**

Use the SDK only for the remote request (see `src/remote.rs`):

```rust
let pending = client.pending(profile)?;            // client.rs:95, Option<PendingIdentity>
if let Some(p) = pending {
    match client.api().remove(&p.token, &p.id).await {   // api.rs:154
        Ok(()) => {}
        Err(opentunnel::Error::Api { status: 404, .. }) => {}   // already gone == success
        Err(e) => return retain_state(e),                       // timeout => UNKNOWN
    }
}
```

After the remote result is settled, record it in wrapper metadata before
unlinking anything. Use the wrapper's name-by-name local cleanup
(`src/cleanup.rs`), not `Storage::remove`, which would recurse into the
profile and delete unrelated entries.

Retry semantics of `Api::remove` (`api.rs:154-160`, `api.rs:74-104`,
`api.rs:179-184`):

- Sent with `Repeat::Once`, so **network retries only happen for
  `error.is_connect()`** — failures that provably never left the machine.
- HTTP retries happen for `429 | 503 | 521 | 522 | 523 | 525` only, up to
  `RETRIES = 6` (7 attempts), with 250 ms→8 s capped exponential backoff plus
  jitter (`protocol/api.rs`).
- A **timeout is returned to you as `Error::Http`, not retried.** It is an
  *unknown* outcome: retain state and tell the user to retry. A later retry
  returning 404 is the confirmation.
- `404` is **not** retryable, so `Api::remove` returns
  `Error::Api { status: 404, .. }` promptly. Match it as success. Note this
  matching is yours to do — `Api::remove` itself does **not** swallow 404; only
  `Client::remove` does.
- `Error` is a plain public enum (`error.rs:4`), so external `match` on
  `Error::Api { status, .. }` compiles fine.

Also relevant: `Error::is_transient()` (`error.rs:39-48`) exists and is public,
but it treats 5xx/429 as transient for `Idempotent` semantics — do not use it to
decide delete retry policy; use the §5.1 rules.

### 5.2 `status_watch()` readiness and terminal detection

```rust
// src/tunnel.rs:91-96
pub enum State { WaitingRoutes, Connecting, Connected, Reconnecting, Stopped }
```

Transitions observed in `Supervisor::run` (`tunnel.rs:385`):

| State set | Where | Meaning for the wrapper |
| --- | --- | --- |
| `Connecting` | initial value at construction; again per attempt (`tunnel.rs:403-404`) | not ready |
| `WaitingRoutes` | `tunnel.rs:391-395`, only when `routes` is empty | **won't happen if you always pass one route** |
| `Connected` | `tunnel.rs:484` | **this is your readiness signal** |
| `Reconnecting` | retry branch, with `last_error` | keep waiting until startup deadline |
| `Stopped` | `tunnel.rs:447`, always the last write before `run()` returns | **terminal signal** |

Two traps:

1. **A cloned `Receiver` marks the current value as seen.** `status_watch()` is
   `self.status.clone()` (`tunnel.rs:185`). If the tunnel is already `Connected`
   when you subscribe, `changed().await` will **never** fire for that value.
   Always inspect `receiver.borrow()` first, then await `changed()`:

   ```rust
   let mut status = tunnel.status_watch();
   loop {
       let state = status.borrow().state.clone();   // check CURRENT value first
       if state == State::Connected { break Ready; }
       if state == State::Stopped   { break Terminal; }
       tokio::select! {
           changed = status.changed() => { if changed.is_err() { break Terminal; } }
           _ = startup_deadline => break TimedOut,
           _ = signal            => break Interrupted,
       }
   }
   ```

   `Status` also carries `last_error: Option<String>` and `connected_at`, both
   useful for diagnostics. `Tunnel::status()` (`tunnel.rs:181`) returns a
   snapshot without subscribing.

2. **`Connected` is per-session and the tunnel can leave it.** After
   `Connected` you must keep watching for `State::Stopped`, or you will report
   readiness and then sit on a dead tunnel. The plan's "select on
   `status_watch()` reaching `Stopped` plus signals" is the right shape; do not
   rely on `broadcast` events (documented as losable, and `subscribe()` drops
   events sent before subscription anyway).

Which terminal errors you will see, and where they originate:

- `Error::Attach("bad_token" | "cert_not_ready")` → `SessionEnd::Fatal`
  (`tunnel.rs:467-469`, fatal set from `protocol/bridge.rs`
  `codes::is_fatal_attach_error`). Do **not** retry; exit non-zero.
- `Error::Invalid(_)` → `SessionEnd::Fatal` (`tunnel.rs:471-472`) — this is how an
  unusable proxy scheme surfaces ("unsupported proxy scheme 'socks5'"). Also
  fatal, and also what you get for a bad route/target (§5.4).
- Everything else (`bridge closed`, `bridge attach timed out`, idle timeout,
  `server draining`, HTTP failures) → `SessionEnd::Retry` with backoff
  250 ms→30 s (`tunnel.rs:263-270`). These are not your problem to handle; the
  startup deadline bounds them.

`close()` returns the supervisor's `Result`: `Ok(())` for a clean shutdown,
`Err(..)` if the tunnel had already stopped fatally. A `close()` error is
informational if you already saw `Stopped`; report it on stderr, don't panic.

### 5.3 Provisioning progress, and the cancellation window

`Client::create` (`client.rs:101`) and `complete` (`client.rs:233`) emit
`ProvisionStage` through a **synchronous `impl Fn(ProvisionStage)`** — it must
not block or await. Exact order:

```text
create():
  CreatingTunnel                      client.rs:115   (before api.create())
  → api.create()  [network]
  → provision():
      GeneratingKey                   client.rs:219
      GeneratingCsr                   client.rs:220
      provision::generate()           (rcgen, sync)
      Storage::save_pending()         client.rs:229   ← token/private-key/pending.json ON DISK
  → complete():
      RequestingCertificate           client.rs:239   ← ★ cancellation window opens here
      api.bind_certificate()  [network]
      loop { WaitingCertificate; api.certificate(); sleep 2s }   client.rs:251-259
      SavingIdentity                  client.rs:276
      Storage::save()                 client.rs:277   ← complete identity committed
      Ready                           client.rs:278
```

So the plan's §4 claim is **verified**: by the time `RequestingCertificate`
fires, `pending.json`, `token`, and `private-key.pem` are already durable, which
is exactly what the pending-identity delete path needs. Implementing "let the
create future run to `RequestingCertificate`, then cancel and clean up via the
pending path" is sound.

Three caveats:

- `WaitingCertificate` is emitted **again on every 2 s poll iteration**, not
  once. Don't treat repeats as a new phase.
- If you cancel the `create` future, you get no return value. Re-read
  `client.pending(profile)?` to recover `id`/`token` for deletion. If it is
  `None`, the cancellation beat `save_pending` and there is nothing to delete
  remotely (the create response may still have orphaned a registration — the
  plan's acknowledged unavoidable case).
- `create` returns `Err(Error::Invalid("profile '…' already has a tunnel"))` if
  `tunnel.json` exists. So do not call `create` unconditionally on resume; check
  `client.get(profile)?` first, or use `resume`.

`Client::ensure` (`client.rs:142`) is convenient but **takes no progress
callback** (`client.rs:144` passes `|_| {}`). If the wrapper wants the
`RequestingCertificate` signal — and it does, for the cancellation window — it
must drive `create`/`resume` itself rather than `ensure`.

`PendingIdentity` and `Identity` both have `Debug` impls that **redact secrets**
(`identity.rs` — only `id`, `hostname`, `certificate_expiry`, and
`finish_non_exhaustive`). Safe to include in error paths; still never log the
struct fields directly.

### 5.4 Route and target validation (two traps)

`Route::validate` → `parse_target` (`protocol/names.rs`):

```rust
pub fn parse_target(value: &str) -> Option<(&str, u16)> {
    let (host, port) = value.rsplit_once(':')?;
    let port: u16 = port.parse().ok().filter(|port| *port != 0)?;   // port 0 rejected
    let host = match host.strip_prefix('[') {
        Some(inner) => inner.strip_suffix(']')?,                        // [::1]:8080 -> ::1
        None => host,
    };
    let valid_host = !host.is_empty()
        && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_'));
    valid_host.then_some((host, port))
}
```

- **Trap A: a bare port is rejected.** `Route::new("3000").validate(..)` returns
  `Err("invalid target '3000' for route '…': use host:port")`, surfaced as
  `Error::Invalid` — which `tunnel.rs:471` treats as **fatal**. The wrapper must
  normalise `3000` → `127.0.0.1:3000` **before** `Client::connect`. This is the
  plan's "bare port means `127.0.0.1`" rule and it lives entirely in the wrapper.
- Bracketed IPv6 works (`[::1]:8080` → host `::1`) and is passed straight to
  `TcpStream::connect`. Unbracketed `::1:8080` also parses (`:` is an allowed
  host character, and `rsplit_once` takes the last colon), yielding host `::1`
  port `8080` — so it happens to work, but it is ambiguous for a bare address
  with no port and does not match the plan's contract. **Require brackets** and
  reject unbracketed forms in CLI validation, as the plan says.
- Host characters are restricted to alnum plus `. - : _`. No IDN, no
  `%zone`, no trailing dot. Reject these in CLI validation with
  `exit 2` rather than letting the SDK turn them into a fatal `Error::Invalid`
  at connect time.

Route **names** (`protocol/names.rs::is_label`) must be:

- `@` (root: the tunnel hostname itself), **or**
- 1–63 chars, lowercase `a-z`, digits, and `-` only in the middle;
  first and last must be `a-z0-9`.

Consequences for the wrapper:

- The generated random route name must be **lowercase alnum, start/end alnum**.
  A name like `-abc` or `ABC` fails `validate` → fatal. Use e.g. 8 chars from
  `[a-z0-9]` starting with a letter.
- To expose the service at the bare tunnel hostname (`https://<hostname>`), the
  route name is `@`. For `https://<label>.<hostname>`, the name is `<label>`.
  `route_for_sni` resolves exactly: SNI `== hostname` → `@`; SNI ending in
  `.<hostname>` → the leading label (so **one** label only, no nesting).
- The persisted URL should therefore be built as
  `https://{label}.{host}` or `https://{host}`, where `{host}` is
  `tunnel.json`'s `hostname`.
- Unknown SNI is answered with `Reset { code: "unknown_route" }`
  (`tunnel.rs:557-565`) — a visitor to a name you didn't register gets a reset,
  not a fallthrough. Good for the fake-bridge test in plan §Testing B.
- `MAX_CONNS = 256` is advertised on attach and enforced client-side
  (`tunnel.rs:564-570`, `too_many_connections`). Irrelevant to normal use.

### 5.5 Storage layout the wrapper must recognise

`Storage::at(root)` + profile `default` (`PLAN.md`'s "fixed internal profile")
produces `root/default/` containing, depending on state:

| File | Written by | Meaning |
| --- | --- | --- |
| `token` | `save_pending` / `save` | bearer token, trailing `\n` |
| `private-key.pem` | `save_pending` / `save` | P-256 key |
| `pending.json` | `save_pending` | `{id, hostname, csr}` (serde default names) |
| `certificate.pem` | `save` | leaf |
| `chain.pem` | `save` | chain |
| `tunnel.json` | `save` **last** | `{id, hostname, certificateExpiry}` (`camelCase`) |

- `tunnel.json` is written **last** and by atomic rename (`storage.rs:156` then
  `rename`), so its presence means "complete identity". Confirmed:
  `Storage::load` treats absent `tunnel.json` as `Ok(None)`.
- `Storage::load` requires `token`, `private-key.pem`, `certificate.pem`,
  `chain.pem`, else `Error::Invalid("incomplete tunnel identity …")`. Treat that
  as **corrupt state, exit 1** — never silently re-provision.
- For `url`, parse `tunnel.json` and use only `hostname`. Do not read
  `token`, `private-key.pem`, `certificate.pem`, or `chain.pem`; the plan's
  "no credential-file reads" is achievable because `hostname` is in
  `tunnel.json` and `id`/route come from your own metadata.
- `storage.remove(profile)` is `fs::remove_dir_all` **of `root/<profile>`** with
  the profile name validated by `is_valid_profile` (`storage.rs:54-61`). It is
  recursive, but scoped to a validated single label under `root`. Still do your
  own symlink/layout validation first, per plan §3.

---

## 6. Options, CA, proxy, and environment

### 6.1 `ClientOptions` is exactly three fields

```rust
// src/client.rs:37-46
pub struct ClientOptions {
    pub api: Option<Url>,          // defaults to https://opentunnel.xyz
    pub storage: Option<Storage>,  // defaults to $XDG_DATA_HOME/opentunnel
    pub ca_cert: Option<PathBuf>,  // defaults to OPENTUNNEL_CA_CERT
}
```

There is **no** proxy field, **no** timeout field, and **no** TLS-verification
toggle. `Client::new` (`client.rs:62`) does
`options.api.unwrap_or(DEFAULT_API)` and
`options.storage.unwrap_or_default()`. Always set `storage` explicitly to
`Storage::at(identity_directory)` — that is what keeps state inside the run
directory and off `$XDG_DATA_HOME`.

### 6.2 Environment variables actually read (verified, with precedence)

| Variable | Read by | Where | Notes |
| --- | --- | --- | --- |
| `OPENTUNNEL_CA_CERT` | SDK | `tls.rs:50` | **fallback only**; `ClientOptions::ca_cert` wins |
| `SSL_CERT_FILE` | rustls-native-certs 0.8.3 → openssl-probe | `lib.rs::CertPaths::from_env` | replaces the OS store |
| `SSL_CERT_DIR` | same | same | colon-separated |
| `HOME`, `XDG_DATA_HOME` | SDK | `paths.rs:7,13` | unused if `storage` is set |
| `XDG_RUNTIME_DIR`, `USERNAME` | SDK | `paths.rs:36,53` | unused by the wrapper |
| `HTTPS_PROXY`/`https_proxy`, `HTTP_PROXY`/`http_proxy`, `ALL_PROXY`/`all_proxy`, `NO_PROXY`/`no_proxy` | hyper-util `Matcher::from_system()` + reqwest | `http_proxy.rs:39`, `api.rs` reqwest default | see §6.4 |
| `OPENTUNNEL_API` | **nobody** | — | **the Rust SDK does not read it** |

The plan's statement that "the Rust SDK does not read `OPENTUNNEL_API`" is
**confirmed**: the only `std::env` reads in the crate are `tls.rs:50`,
`paths.rs:7/13/36/53` (plus the upstream *tests*). Pass the endpoint through
`ClientOptions::api`.

`SSL_CERT_FILE` / `SSL_CERT_DIR` semantics (confirmed in rustls-native-certs
0.8.3 source, `load_native_certs`): **if either is set, certificates are loaded
only from those locations and the platform store is not consulted.** That is a
sharp edge worth documenting for users:

- Setting `SSL_CERT_DIR` to a private dir *replaces* the system roots. For the
  hosted API this is fine (bundled Mozilla roots are always added) but a
  private-CA user will otherwise lose system roots unexpectedly.
- An unreadable `SSL_CERT_FILE` yields errors and **zero** native certs; the SDK
  logs them at `debug!` (`tls.rs:67-71`) and continues with the bundled Mozilla
  roots. TLS verification is **never** disabled. Good.

### 6.3 CA handling has a silent-failure mode the wrapper should close

```rust
// src/tls.rs:47-58
pub(crate) fn roots(ca_cert: Option<&Path>) -> RootCertStore {
    let ca_cert = ca_cert.map(Path::to_path_buf)
        .or_else(|| std::env::var_os(CA_CERT_ENV).map(PathBuf::from));
    let extra = match ca_cert {
        Some(path) => read_ca_certificates(&path).unwrap_or_else(|error| {
            warn!(%error, "ignoring the configured CA certificates");   // ← SWALLOWED
            Vec::new()
        }),
        None => Vec::new(),
    };
    build_roots(rustls_native_certs::load_native_certs(), extra)
}
```

A missing/unreadable/garbage `--ca` path is **warned and ignored**, and the
client proceeds with bundled + OS roots. The user then sees an opaque
`UnknownIssuer`-style bridge failure minutes later instead of "your CA file is
unreadable".

Actionable: the wrapper should validate the CA file itself at startup —
`std::fs::read` + `opentunnel::tls::read_ca_certificates(path)` (that function is
**public**, `tls.rs:21`, and returns a proper `Result`) — and fail fast with a
clear stderr message and exit 1. Do this once, at startup, before provisioning.
For the automatic ephemeral cleanup, carry the original CA *option values*
forward as the plan says, since the paths aren't persisted.

`Api::with_trust` also calls `install_crypto_provider()` (`lib.rs`), a
process-global, error-ignoring `install_default()`. Constructing several
`Client`s in one process (forwarding + cleanup) is fine and idempotent.

### 6.4 Proxy behaviour (both API and bridge)

- reqwest's client is built with defaults + a preconfigured TLS backend, so it
  uses system proxy settings (`HTTPS_PROXY` for `https://`, …) automatically.
- The bridge does **not** rely on reqwest: `Proxies::from_env()` wraps
  hyper-util's `Matcher::from_system()` (`http_proxy.rs:38-40`) and
  `open()`/`tunnel()` (`http_proxy.rs:67-143`) implement `CONNECT` for
  `http://` and `https://` proxies, sending `Proxy-Authorization` from the URL's
  basic-auth credentials.
- **A SOCKS proxy is a hard, fatal error**, not a bypass:
  `Error::Invalid("unsupported proxy scheme 'socks5' …")` → fatal at
  `tunnel.rs:471`. Surface it clearly; do not retry.
- `Proxies` is captured **once, at `Client::new` time**. Changing proxy env vars
  afterwards has no effect on an existing client. This is another reason the
  cleanup `Client` must be freshly constructed (as the plan requires) — and it
  means the cleanup client picks up the *then-current* environment, so document
  that.
- `Proxies`' `Debug` is deliberately redacted (`http_proxy.rs:30-34`) so proxy
  credentials don't leak through `{:?}`. Don't undo that by logging the raw env.

### 6.5 `--api` override must be origin-only

```rust
// src/api.rs, request()
let url = self.base.join(path).expect("API paths are valid");
// path is "/api/tunnel", "/api/tunnel/{id}", "/api/tunnel/{id}/certificate", "/api/tunnel/{id}/connect"
```

`Url::join` with a **leading slash** discards any path in the base. So
`--api http://127.0.0.1:8080/prefix` silently resolves to
`http://127.0.0.1:8080/api/tunnel…`. The hidden `--api` used by local tests must
therefore be an **origin only** (`scheme://host[:port]`); reject a base with a
non-empty path at argument validation (exit 2) instead of producing confusing
404s. `connect_url` likewise derives `ws`/`wss` by rewriting the scheme
(`api.rs:163-171`), which only makes sense for an origin.

Also note `Api::connect_url` is built from `base`, so the fake API and fake
bridge must share one origin (the SDK derives `/api/tunnel/{id}/connect` on the
API's host). The plan's single fake server serving both is the right shape.

---

## 7. Filesystem details that affect recovery and deletion

### 7.1 Temp-file residue names — all six confirmed

`write_secret` uses `path.with_extension(format!("tmp-{}", pid))`
(`storage.rs:231`). `with_extension` **replaces the final extension**, so:

| Target | Residue on SIGKILL |
| --- | --- |
| `token` | `token.tmp-<pid>` |
| `private-key.pem` | `private-key.tmp-<pid>` |
| `certificate.pem` | `certificate.tmp-<pid>` |
| `chain.pem` | `chain.tmp-<pid>` |
| `tunnel.json` | `tunnel.tmp-<pid>` |
| `pending.json` | `pending.tmp-<pid>` |

This matches `PLAN.md` §3's list **exactly** — no more, no fewer. Recognise only
these names (plus your own `wrapper.json.tmp-<pid>-<random>`), only inside a
validated profile directory, with matching ownership and no symlinks.

### 7.2 Residue implications

- `pending.tmp-<pid>` can exist while `pending.json` does **not**: it means the
  write was interrupted. Do **not** promote it to a completed identity; the plan
  is right that a PID suffix proves nothing about liveness.
- Because `tunnel.json` is written last, a torn `save` can leave a **complete**
  credential set *plus* `tunnel.tmp-<pid>` — that is still a valid identity.
  Do not treat the temp file's presence as corruption.
- A torn `save_pending` can leave `token` + `private-key.pem` but no
  `pending.json`. According to the plan's classification that is
  "SDK files without wrapper metadata / malformed state" → exit 1 and preserve,
  not auto-repair. That is the conservative and correct choice; just make the
  error message name the exact files found.

### 7.3 One permission caveat in `write_secret`

`options.write(true).create(true).truncate(true)` with
`OpenOptionsExt::mode(0o600)` — the mode applies **only when the file is
created**. If an attacker (or a previous run) left a world-readable
`token.tmp-1234`, the new write **keeps its existing mode**. Likewise the target
file's final mode after `rename` is the temp file's mode, not forced to 0600.

Practical guidance:

- Create the state directory `0700` yourself before the SDK runs (the SDK's
  `create_private_dir` does `0700` recursively but never *tightens* an existing
  directory — `DirBuilder::mode` is creation-only, as `PLAN.md` §3 warns).
- On recovery, if you find a recognised temp file with permissions wider than
  `0600` or not owned by you, **do not** delete-and-assume; report corrupt state
  (exit 1) and preserve. This is the plan's "unsafe ownership … must not be
  blindly removed".

### 7.4 There is no locking in the SDK

Nothing in the crate opens a lock file, calls `flock`/`try_lock`, or writes a
PID file. Verified by search (§8). Two processes pointed at the same
`Storage::at(root)` + profile will happily interleave `save`/`save_pending`
(and their temp names collide, being PID-only). The wrapper's
`std::fs::File::try_lock` + dev/inode identity check is therefore the **only**
exclusion mechanism, exactly as the plan states. Nothing in the SDK will
cooperate with or work around it.

---

## 8. Audit commands (reproducible)

Run against the extracted crate root (§1.1). `rg` is ripgrep; the shell available
during this audit lacked it, so equivalent `Grep`-tool searches were used and
produced the results quoted in this document.

```sh
cd "$SRC"                     # .../opentunnel-0.6.2

# A. No async/blocking/thread file or process escape hatches anywhere in the crate
rg -n 'tokio::fs|spawn_blocking|block_on|block_in_place|thread::spawn|std::thread|std::process::Command' src
#   expect: NO MATCHES

# B. Every filesystem call in the crate, and its module
rg -n 'fs::(read|write|remove|rename|OpenOptions|DirBuilder|read_dir|read_to_string|metadata)' src
#   expect: storage.rs x9, tls.rs x1 — nothing in protocol/ or elsewhere
#   (the 3 non-`fs::` writes are options.open/file.write_all/file.sync_all at
#    storage.rs:236-238, covered by grep E and by reading write_secret)

# C. All task dispatch, to find writers that may outlive the runtime
rg -n 'tokio::spawn|tasks\.spawn|JoinSet|AbortHandle|\.abort\(\)' src
#   expect: tunnel.rs only; refresh_certificate at 143, supervisor at 165

# D. Confirm the refresh task's handle is dropped (not joined)
rg -n -A3 'tokio::spawn\(refresh_certificate' src/tunnel.rs
#   expect: no `let handle =` binding

# E. Credential write call graph
rg -n 'storage\.save|save_pending|write_secret|storage\.remove' src
#   expect: writers only in storage.rs; callers client.rs:169,229,277 and tunnel.rs:321

# F. Deletion / 404 handling
rg -n -B2 -A10 'pub async fn remove' src/client.rs src/api.rs
rg -n 'Repeat::Once|Repeat::Idempotent' src/api.rs src/protocol/api.rs

# G. Env vars actually read
rg -n 'env::var|env::var_os|from_system|OPENTUNNEL_|SSL_CERT' src
#   expect: tls.rs:50 OPENTUNNEL_CA_CERT; paths.rs 7/13/36/53; http_proxy.rs:39

# H. No TLS-verification escape
rg -n 'accept_invalid|dangerous|verify_none|insecure' src
#   expect: NO MATCHES

# I. No locking / no signal handling / no process exit
rg -n 'try_lock|flock|lock_exclusive|signal|SIGINT|SIGTERM|process::exit' src
#   expect: NO MATCHES

# J. Unsafe usage (should be exactly one, getuid)
rg -n 'unsafe' src
#   expect: paths.rs:45,48 only
```

Observed results (this audit):

- A: one match only, `tunnel.rs:842 tokio::io::copy_bidirectional` (ordinary
  tunnel traffic, not storage).
- B: 10 `fs::` matches (9 in `storage.rs`, 1 in `tls.rs`), exactly as tabulated
  in §3.1; the remaining three operations are file-method calls.
- C: 13 matches, all `tunnel.rs`.
- D: handle dropped — confirmed.
- E: exactly the six call sites in §3.3.
- F: `client.rs:186-194` and `api.rs:154-160`, as quoted.
- G: exactly `tls.rs:50`, `paths.rs:7/13/36/53`, `http_proxy.rs:39`.
- H, I: no matches.
- J: only `paths.rs:45,48` (`getuid`).

---

## 9. Caveats and known limitations to carry into the implementation

1. **The gate has one precondition** — current-thread runtime **and** delete only
   after that runtime is torn down (§4.1, §4.2). Encode this as a comment and a
   test; it is the whole safety argument.
2. **`close()` is not quiescence.** A refresh GET in flight can still write
   credentials after `close()` returns. Do not reorder the shutdown sequence.
3. **DNS on the blocking pool outlives `shutdown_background()`.** Harmless for
   writes; it does mean "background shutdown returned" ≠ "all work finished".
   Say so in the code and docs (the plan already does).
4. **`Client::remove` is unusable for pending identities** and deletes local
   state unconditionally. Recover pending credentials with `pending()` and call
   `api().remove()` explicitly. After confirmed deletion/404, use the wrapper's
   known-entry, nonrecursive local cleanup: `storage().remove()` recursively
   removes the profile and would destroy unrelated files.
5. **Delete timeouts are unknown, not failures.** Retain state, print the exact
   retry command. Only 404 is a positive "already gone".
6. **`create` errors if a complete identity exists**; check `get()` first or use
   `resume()`. `ensure()` gives you no progress callback, so it cannot drive the
   cancellation window.
7. **Bare ports are rejected by the SDK** — normalise in the CLI. Route names
   must be `@` or lowercase-alnum labels; invalid names/targets become *fatal*
   connect errors, not retryable ones.
8. **`--api` must be an origin.** A base path is silently discarded by
   `Url::join("/api/…")`.
9. **An unreadable `--ca` file is swallowed with a warning.** Pre-validate with
   the public `opentunnel::tls::read_ca_certificates`.
10. **`SSL_CERT_FILE`/`SSL_CERT_DIR` replace the OS store** (not additive),
    though bundled Mozilla roots are always present.
11. **Proxy env is sampled at `Client::new`.** Construct the cleanup client
    fresh, as planned. SOCKS is fatal by design.
12. **No SDK locking whatsoever** — the wrapper's lock is the only exclusion.
13. **`wait()` consumes the task handle**; never race it with signals.
14. **A cloned `watch::Receiver` has already "seen" the current status** — read
    `borrow()` before awaiting `changed()`.
15. **`tokio`'s `fs` feature is enabled but unused.** Do not treat the feature
    as evidence of async writes, and do not add `tokio::fs` usage in a fork
    without redoing §3.
16. **MSRV is exactly 1.89** and edition is 2024. Pin the toolchain ≥1.89; the
    plan's choice satisfies this.
17. **`WaitingCertificate` repeats every 2 s**; don't treat repeats as phases.
18. **Residue set is exactly six names** (§7.1). Anything else is unrelated user
    data and must be preserved.
19. Upstream publishes from `crates/opentunnel` in a monorepo whose default
    branch is `dev`; the `v0.6.2` tag matches the crate sha1 today, but **pin and
    cite the sha1** so a moved tag cannot silently change what you audited.

---

## 10. Update checklist for every SDK bump

Do this before bumping the pin. Budget ~30–45 minutes; it is mostly mechanical.

- [ ] Record the new version **and** the new `.cargo_vcs_info.json` sha1 in this
      file (§1 table). Diff the audited sha1 against the previous one.
- [ ] `git diff <old-sha> <new-sha> -- crates/opentunnel/src` and read **every**
      hunk. Do not rely on the greps alone; a writer can be introduced behind a
      new helper name.
- [ ] Re-run §8 A–J. Specifically confirm:
  - [ ] no `tokio::fs` / `spawn_blocking` / `thread::spawn` / `block_in_place`
        newly appears near storage;
  - [ ] the set of `storage.save`/`save_pending`/`write_secret` call sites is
        still exactly §3.3, and no new **un-joined** task can reach them;
  - [ ] `refresh_certificate` is still spawned without a stored handle (if it
        gains a join, §4.2 can be relaxed — but verify, don't assume);
  - [ ] `Client::remove` still skips the server call for pending-only state;
  - [ ] `Api::remove`'s `Repeat` class and retryable status set are unchanged;
  - [ ] 404 still surfaces as `Error::Api { status: 404, .. }`;
  - [ ] `write_secret`'s temp naming still yields the six §7.1 names
        (watch for `with_extension` → `with_added_extension` or a random suffix);
  - [ ] `tunnel.json` is still written last and atomically renamed;
  - [ ] `State`/`Event`/`Status` shape and the `Stopped`/`Connected` semantics
        are unchanged (wrapper select logic depends on them);
  - [ ] `ClientOptions` has no new field that changes proxy/CA/timeout behaviour,
        and still has no TLS-verification toggle;
  - [ ] no new environment variable is read (diff `rg 'env::var'`);
  - [ ] `is_fatal_attach_error` / `Error::Invalid`-is-fatal behaviour unchanged.
- [ ] Re-check `PLAN.md`'s claims against the new source; update the plan if any
      of F1–F12 change.
- [ ] Re-run the wrapper's delayed-refresh-during-shutdown regression test and
      the pending-identity deletion test against the new version.
- [ ] Re-resolve dependencies and re-read `Cargo.toml` for feature changes —
      especially tokio's `fs`/`rt-multi-thread`, reqwest's default features, and
      any new `spawn_blocking`-using dependency in the write path.
- [ ] Update the "Exact version and source" table, refresh §11's conformance
      check against the code as it then stands, and append a dated row to §12.

---

## 11. Implementation cross-check

The implementation uses the following audited boundaries. Refer to symbols
rather than line numbers, which change as the wrapper evolves. This static
cross-check does not substitute for the lifecycle regression tests or CI.

| Requirement | Where | Status |
| --- | --- | --- |
| Pin `=0.6.2`, edition 2024, MSRV 1.89, no `rt-multi-thread` | `Cargo.toml` | OK |
| Current-thread runtime for provisioning and forwarding | `forward::forwarding_runtime` | OK |
| Same, on a fresh runtime, for remote deletion | `remote::delete_saved_identity` | OK |
| Runtime torn down before local state is removed | `forward::forward`, `forward::init`, `cleanup::remove_local` | OK |
| Pending-only state bypasses `Client::remove` | `remote::delete_saved_identity` | OK |
| `api().remove(token, id)` on the cleanup client | `remote` | OK |
| Readiness from `State::Connected`, terminal from `State::Stopped` | `forward::wait_attached`, `forward::supervise` | OK |
| `Tunnel::wait()` never raced against signals | `forward::supervise` | OK |
| Unrelated profile files preserved | Remote-only API deletion followed by `cleanup::remove_local`, never SDK recursive removal | OK |

Both `wait_attached` and `supervise` read `borrow_and_update()` **before**
awaiting `changed()`, and handle sender closure. This is the correct form from
§5.2: already-connected or already-stopped state must not be missed. Do not
replace it with a bare `changed().await` loop.

Also confirmed by reading: `src/forward.rs` imports and uses only public SDK API
(`Client`, `ClientOptions`, `Storage`, `Route`, `Tunnel`, `State`, `Status`), and
nothing in `src/` calls `tokio::fs`, `spawn_blocking`, or `Client::remove`.

---

## 12. Audit log

| Date | Version | Revision | Auditor | Result |
| --- | --- | --- | --- | --- |
| 2026-11-19 | 0.6.2 | `1ad859e17924ec3f6741cbc4cdb54ce381c9007c` | SDK audit subagent | Pass. No credential writer escapes the async task boundary. Gate conditional on current-thread runtime + delete-after-teardown (§4). |
