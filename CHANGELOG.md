# Changelog

This file records the notable changes to each release of the published crates.
Every published crate carries the same version. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the versions
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `Capability::LateTrapReasons` (`late_trap_reasons`), which the browser backend
  declares with host suspension: a call that traps learns why only on a
  microtask. `SuspendProvider::reports_traps_late` carries it to the scheduler.

### Fixed

- A trap in a synchronous-typed export is the call's own error on the browser
  backend: the export runs as a plain core call, so the call is `Ready(Err)` on
  its first poll instead of `Pending` until a microtask.

### Performance

- `canon context.get` and `canon context.set` no longer cross into the host. A
  small core module of the store holds the running thread's two slots in
  mutable `i32` globals and exports the functions that read and write them, and
  the store links those in place of the built-ins, so a guest's call stays in
  WebAssembly. A guest that keeps its shadow stack pointer in a slot calls them
  for every function with a frame, which cost about 30 to 50 times in the
  browser. The store makes the globals the slots of the thread that runs before
  guest code gets control: ahead of every call into the guest and as every host
  function returns to it. A thread's record holds its slots only while it is not
  the running thread.

### Changed

- `Store::run_until_stalled` answers a `Stalled`: `Yielded` when a task gave way
  and is ready to run again, `Idle` when the store runs again only after
  external work. A host that drives in bursts needs the difference, because a
  yield left undriven starves that task and every task spawned behind it.

## [0.1.1] - 2026-10-21

### Performance

- `wcmp-wasm-core-web` no longer decodes every function body of a module to
  learn whether it waits or notifies. A module whose `target_features` section
  names no atomics cannot, so the browser backend skips its bodies. A module
  without the section is read as before. This removes about 70 of 105 ms from
  the load of a 10 MB module in Chrome. By Irakli Gozalishvili in
  [#1](https://github.com/dialog-db/wasm-component-model-polyfill/pull/1).

[0.1.1]: https://github.com/dialog-db/wasm-component-model-polyfill/releases/tag/v0.1.1
