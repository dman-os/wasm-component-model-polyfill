// Copyright 2026 The Dialog DB Project
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Exists so cargo sets `OUT_DIR` for this crate: `include_generated_code_from_file`
//! and `debug` write the generated code there, read with `env!("OUT_DIR")`. A
//! custom `rustc-env` copy of the path would land in compile-cache keys.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
}
