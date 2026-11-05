// Copyright 2026 The Dialog DB Project
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Baseline tests for the context slots across threads that
//! interleave.
//!
//! `context.get` and `context.set` read and write the slots of the
//! thread that runs. A thread that suspends keeps the values it set,
//! another thread that runs meanwhile starts with zeros and sets
//! values of its own, and the first thread sees only its own once it
//! goes on. The polyfill keeps the slots of the running thread in
//! core globals that the guest reads without leaving WebAssembly, so
//! these tests are what show that every switch of the running thread
//! saves the globals of the thread that leaves and loads the globals
//! of the thread that enters.

#![cfg(test)]

use crate::{Component, Engine, EngineConfig, Func, Instance, Linker, Store, Val};
use wcmp_macros::component;

/// One component instance with two exports that each run two threads
/// against the same context slots.
///
/// The export's own thread sets slot 0 to 7 and slot 1 to 70, and
/// then waits for a thread it started: `suspend-resume-later` suspends
/// and waits for the thread to resume it, and `suspend-then-resume`
/// switches to the thread outright. The thread reads both slots, adds
/// them into the global `$seen`, sets slot 0 to 9 and slot 1 to 90,
/// and makes the export's thread ready again. The export's thread
/// answers with its own slot 0, its own slot 1 times 100, and what
/// the other thread saw times 10,000. A slot of the export's thread
/// that the other thread changed would show in the low digits, and a
/// slot of the other thread that the export's thread passed on would
/// show in the high digits.
const INTERLEAVED: &[u8] = component!(
    r#"
    (component
      (core module $libc
        (memory (export "memory") 1)
        (table (export "__indirect_function_table") 1 funcref))
      (core instance $libc (instantiate $libc))

      (core func $thread-index (canon thread.index))
      (core type $start-ty (func (param i32)))
      (alias core export $libc "__indirect_function_table" (core table $table))
      (core func $new-indirect (canon thread.new-indirect $start-ty (core table $table)))
      (core func $resume-later (canon thread.resume-later))
      (core func $suspend (canon thread.suspend))
      (core func $suspend-then-resume (canon thread.suspend-then-resume))
      (core func $get0 (canon context.get i32 0))
      (core func $get1 (canon context.get i32 1))
      (core func $set0 (canon context.set i32 0))
      (core func $set1 (canon context.set i32 1))

      (core module $m
        (import "" "thread.index" (func $thread-index (result i32)))
        (import "" "thread.new-indirect" (func $new-indirect (param i32 i32) (result i32)))
        (import "" "thread.resume-later" (func $resume-later (param i32)))
        (import "" "thread.suspend" (func $suspend (result i32)))
        (import "" "thread.suspend-then-resume" (func $suspend-then-resume (param i32) (result i32)))
        (import "" "context.get0" (func $get0 (result i32)))
        (import "" "context.get1" (func $get1 (result i32)))
        (import "" "context.set0" (func $set0 (param i32)))
        (import "" "context.set1" (func $set1 (param i32)))
        (import "libc" "__indirect_function_table" (table 1 funcref))

        (global $main (mut i32) (i32.const 0))
        (global $seen (mut i32) (i32.const 0))

        (func $other (param i32)
          (global.set $seen (i32.add (call $get0) (call $get1)))
          (call $set0 (i32.const 9))
          (call $set1 (i32.const 90))
          (call $resume-later (global.get $main)))
        (elem (table 0) (i32.const 0) func $other)

        (func $begin
          (global.set $main (call $thread-index))
          (global.set $seen (i32.const 0))
          (call $set0 (i32.const 7))
          (call $set1 (i32.const 70)))
        (func $end (result i32)
          (i32.add
            (i32.add (call $get0) (i32.mul (call $get1) (i32.const 100)))
            (i32.mul (global.get $seen) (i32.const 10000))))

        (func (export "suspend-resume-later") (result i32)
          (call $begin)
          (call $resume-later (call $new-indirect (i32.const 0) (i32.const 0)))
          (drop (call $suspend))
          (call $end))
        (func (export "suspend-then-resume") (result i32)
          (call $begin)
          (drop (call $suspend-then-resume (call $new-indirect (i32.const 0) (i32.const 0))))
          (call $end)))

      (core instance $i (instantiate $m
        (with "" (instance
          (export "thread.index" (func $thread-index))
          (export "thread.new-indirect" (func $new-indirect))
          (export "thread.resume-later" (func $resume-later))
          (export "thread.suspend" (func $suspend))
          (export "thread.suspend-then-resume" (func $suspend-then-resume))
          (export "context.get0" (func $get0))
          (export "context.get1" (func $get1))
          (export "context.set0" (func $set0))
          (export "context.set1" (func $set1))))
        (with "libc" (instance $libc))))

      (func (export "suspend-resume-later") (result u32)
        (canon lift (core func $i "suspend-resume-later")))
      (func (export "suspend-then-resume") (result u32)
        (canon lift (core func $i "suspend-then-resume"))))
    "#
);

/// Call the export `name` of [`INTERLEAVED`] in a fresh store of an
/// engine with the thread built-ins allowed, and answer what it
/// returned.
async fn answer_of(name: &str) -> u32 {
    let mut config = EngineConfig::new();
    config.wasm_component_model_threading(true);
    let engine = Engine::with_backend(crate::runtime_layer::test_backend())
        .and_then(|engine| engine.with_config(&config))
        .expect("engine");
    let component = Component::new(&engine, INTERLEAVED)
        .await
        .expect("component parses");
    let mut store: Store<()> = Store::new(&engine, ()).expect("store");
    let linker: Linker<()> = Linker::new(&engine);
    let instance: Instance = linker
        .instantiate(&mut store, &component)
        .await
        .expect("instantiate");
    let func: Func = instance.get_func(name).expect("the export is declared");
    match func.call(&mut store, &[]).await {
        Ok(values) => match values.first() {
            Some(Val::U32(value)) => *value,
            other => panic!("{name} answered {other:?}"),
        },
        Err(error) => panic!("{name} failed: {error}"),
    }
}

#[wcmp_macros::test]
async fn it_gives_each_thread_its_own_slots_while_another_runs_between_a_suspension_and_its_resume()
{
    assert_eq!(
        answer_of("suspend-resume-later").await,
        7 + 7000,
        "the thread that started saw the zeros of a fresh thread, and the \
         suspended thread saw 7 and 70 once the other set 9 and 90"
    );
}

#[wcmp_macros::test]
async fn it_gives_each_thread_its_own_slots_across_a_switch_that_starts_the_other() {
    assert_eq!(
        answer_of("suspend-then-resume").await,
        7 + 7000,
        "the thread switched to saw the zeros of a fresh thread, and the \
         thread that switched saw 7 and 70 once the other set 9 and 90"
    );
}
