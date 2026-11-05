// Copyright 2026 The Dialog DB Project
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Baseline tests for a synchronous-typed export on a backend that
//! declares `late_trap_reasons`.
//!
//! The browser reports why a resumable call trapped only once the
//! call's promise settles, so a trap in an export started through the
//! provider's thread is `Running` until a microtask, and the export's
//! call is `Pending` on its first poll. A synchronous-typed export
//! cannot suspend (its instance holds the may-not-suspend flag), so a
//! provider that reports late traps starts it as a plain core call,
//! which has the trap before it returns.
//!
//! The tests run on a Wasmi backend that declares the capability,
//! natively, where the provider would otherwise start the export
//! through a resumable call that learns of the trap at once, and in the
//! browser on the browser's own backend, which declares it.

#![cfg(test)]

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use wcmp_macros::component;

use crate::concurrency::{HostSuspensionProvider, SuspendProvider};
use crate::internal::EngineInternal;
use crate::runtime_layer::{Backend, test_suspending_backend};
use crate::store::StoreInternalExt;
use crate::{Component, Engine, Error, Instance, Linker, Store, Val};

/// A component of three synchronous exports and one asynchronous one.
///
/// - `ok` answers 9, and `trap` traps.
/// - `block` waits on a waitable set nothing can ever wake, which a
///   synchronous export may not do.
/// - `drive` is an `async` callback export that yields once and
///   resolves with 8, so it runs through the provider's thread.
const EXPORTS: &[u8] = component!(
    r#"
    (component
      (core module $libc (memory (export "memory") 1))
      (core instance $libc (instantiate $libc))
      (core func $task-return (canon task.return (result u32)))
      (core func $set-new (canon waitable-set.new))
      (core func $wait (canon waitable-set.wait (memory (core memory $libc "memory"))))
      (core module $m
        (import "libc" "memory" (memory 1))
        (import "" "task.return" (func $task-return (param i32)))
        (import "" "waitable-set.new" (func $set-new (result i32)))
        (import "" "waitable-set.wait" (func $wait (param i32 i32) (result i32)))
        (func (export "ok") (result i32) (i32.const 9))
        (func (export "trap") unreachable)
        (func (export "block") (result i32)
          (drop (call $wait (call $set-new) (i32.const 0)))
          (i32.const 0))
        (func (export "drive") (result i32)
          ;; Yield.
          (i32.const 1))
        (func (export "drive-callback") (param i32 i32 i32) (result i32)
          (call $task-return (i32.const 8))
          ;; Exit.
          (i32.const 0)))
      (core instance $i (instantiate $m
        (with "libc" (instance $libc))
        (with "" (instance
          (export "task.return" (func $task-return))
          (export "waitable-set.new" (func $set-new))
          (export "waitable-set.wait" (func $wait))))))
      (func (export "ok") (result u32) (canon lift (core func $i "ok")))
      (func (export "trap") (canon lift (core func $i "trap")))
      (func (export "block") (result u32) (canon lift (core func $i "block")))
      (func (export "drive") async (result u32)
        (canon lift (core func $i "drive") async
          (callback (core func $i "drive-callback")))))
    "#
);

/// The backend of the suspension tests, which declares
/// `late_trap_reasons` where it does not already. The browser's backend
/// declares it, so this only adds it to Wasmi.
#[cfg(not(target_arch = "wasm32"))]
fn late_trap_backend() -> impl Backend {
    use crate::runtime_layer::{Capabilities, Capability};
    use wcmp_wasm_core::backend::{BackendModule, BackendStore, BoxFuture, StoreData};

    struct LateTraps<B>(B);

    impl<B: Backend> Backend for LateTraps<B> {
        fn capabilities(&self) -> Capabilities {
            self.0.capabilities().with(Capability::LateTrapReasons)
        }

        fn compile<'a>(
            &'a self,
            bytes: &'a [u8],
        ) -> BoxFuture<'a, wcmp_wasm_core::Result<Box<dyn BackendModule>>> {
            self.0.compile(bytes)
        }

        fn compile_sync(&self, bytes: &[u8]) -> wcmp_wasm_core::Result<Box<dyn BackendModule>> {
            self.0.compile_sync(bytes)
        }

        fn new_store(&self, data: StoreData) -> wcmp_wasm_core::Result<Box<dyn BackendStore>> {
            self.0.new_store(data)
        }
    }

    LateTraps(test_suspending_backend())
}

#[cfg(target_arch = "wasm32")]
fn late_trap_backend() -> impl Backend {
    test_suspending_backend()
}

/// Poll `future` once with a waker that does nothing.
fn poll_once<F: Future>(future: &mut Pin<Box<F>>) -> Poll<F::Output> {
    future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
}

async fn instantiate(engine: &Engine) -> (Store<()>, Instance) {
    let linker: Linker<()> = Linker::new(engine);
    let component = Component::new(engine, EXPORTS)
        .await
        .expect("component parses");
    let mut store: Store<()> = Store::new(engine, ()).expect("store");
    let instance = linker
        .instantiate(&mut store, &component)
        .await
        .expect("the component instantiates");
    (store, instance)
}

#[wcmp_macros::test]
async fn it_reports_traps_late_where_the_backend_declares_late_trap_reasons() {
    let late = Engine::with_backend(late_trap_backend()).expect("engine");
    let mut store: Store<()> = Store::new(&late, ()).expect("store");
    let mut context = store.internal().context();
    let provider = HostSuspensionProvider::instantiate(&mut context, late.switch_modules())
        .expect("the backend declares host suspension");
    assert!(SuspendProvider::<()>::reports_traps_late(&provider));

    // Natively, Wasmi's own resumable calls learn of a trap at once.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let plain = Engine::with_backend(test_suspending_backend()).expect("engine");
        let mut store: Store<()> = Store::new(&plain, ()).expect("store");
        let mut context = store.internal().context();
        let provider = HostSuspensionProvider::instantiate(&mut context, plain.switch_modules())
            .expect("the backend declares host suspension");
        assert!(!SuspendProvider::<()>::reports_traps_late(&provider));
    }
}

#[wcmp_macros::test]
async fn it_fails_a_trapping_synchronous_export_on_its_first_poll() {
    let engine = Engine::with_backend(late_trap_backend()).expect("engine");
    let (mut store, instance) = instantiate(&engine).await;
    let trap = instance.get_func("trap").expect("`trap` is exported");

    let mut call = Box::pin(trap.call(&mut store, &[]));
    let Poll::Ready(Err(error)) = poll_once(&mut call) else {
        panic!("a trapping synchronous export answers `Ready(Err)` on its first poll");
    };
    drop(call);
    assert!(
        error.to_string().contains("unreachable"),
        "the call fails with the trap, got {error}"
    );
}

#[wcmp_macros::test]
async fn it_answers_a_synchronous_export_and_an_async_one_as_before() {
    let engine = Engine::with_backend(late_trap_backend()).expect("engine");
    let (mut store, instance) = instantiate(&engine).await;

    for _ in 0..2 {
        let ok = instance.get_func("ok").expect("`ok` is exported");
        let mut call = Box::pin(ok.call(&mut store, &[]));
        let Poll::Ready(answered) = poll_once(&mut call) else {
            panic!("a synchronous export answers on its first poll");
        };
        drop(call);
        assert_eq!(answered.expect("`ok` answers").as_ref(), [Val::U32(9)]);
    }

    let drive = instance.get_func("drive").expect("`drive` is exported");
    let driven = drive
        .call(&mut store, &[])
        .await
        .expect("an async callback export still runs through the provider");
    assert_eq!(driven.as_ref(), [Val::U32(8)]);
}

#[wcmp_macros::test]
async fn it_fails_a_blocking_synchronous_export_as_it_does_without_the_capability() {
    async fn blocked(engine: &Engine) -> String {
        let (mut store, instance) = instantiate(engine).await;
        let block = instance.get_func("block").expect("`block` is exported");
        let error: Error = block
            .call(&mut store, &[])
            .await
            .expect_err("a synchronous export may not block");
        error.to_string()
    }

    let late = blocked(&Engine::with_backend(late_trap_backend()).expect("engine")).await;
    let reference =
        blocked(&Engine::with_backend(test_suspending_backend()).expect("engine")).await;
    assert_eq!(
        late, reference,
        "the plain call fails a block with the cause the provider's thread does"
    );
}
