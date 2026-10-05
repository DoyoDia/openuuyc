// Copyright (c) 2017-2022, The rav1e contributors. All rights reserved
//
// This source code is subject to the terms of the BSD 2 Clause License and
// the Alliance for Open Media Patent License 1.0. If the BSD 2 Clause License
// was not distributed with this source code in the LICENSE file, you can
// obtain it at www.aomedia.org/license/software. If the Alliance for Open
// Media Patent License 1.0 was not distributed with this source code in the
// PATENTS file, you can obtain it at www.aomedia.org/license/patent.

#![allow(clippy::print_literal)]
#![allow(clippy::unused_io_amount)]

#[allow(unused_imports)]
use std::env;
use std::fs;
use std::path::Path;

#[cfg(feature = "asm")]
fn build_nasm_files() {
    let mut config = "
%pragma preproc sane_empty_expansion true
%define private_prefix rav1e
%define ARCH_X86_32 0
%define ARCH_X86_64 1
%define PIC 1
%define STACK_ALIGNMENT 16
%define HAVE_AVX512ICL 1
"
    .to_owned();

    if env::var("CARGO_CFG_TARGET_VENDOR").unwrap() == "apple" {
        config += "%define PREFIX 1\n";
    }

    let out_dir = Path::new(&env::var("OUT_DIR").unwrap()).join("encoder");
    fs::create_dir_all(&out_dir).unwrap();
    let out_dir = out_dir.to_str().unwrap();
    let dest_path = Path::new(&out_dir).join("config.asm");
    std::fs::write(&dest_path, config).expect("can write config.asm");

    let asm_files = &[
        "src/encoder/x86/cdef_avx2.asm",
        "src/encoder/x86/cdef_avx512.asm",
        "src/encoder/x86/cdef_dist.asm",
        "src/encoder/x86/cdef_rav1e.asm",
        "src/encoder/x86/cdef_sse.asm",
        "src/encoder/x86/cdef16_avx2.asm",
        "src/encoder/x86/cdef16_avx512.asm",
        "src/encoder/x86/cdef16_sse.asm",
        "src/encoder/x86/ipred_avx2.asm",
        "src/encoder/x86/ipred_avx512.asm",
        "src/encoder/x86/ipred_sse.asm",
        "src/encoder/x86/ipred16_avx2.asm",
        "src/encoder/x86/ipred16_avx512.asm",
        "src/encoder/x86/ipred16_sse.asm",
        "src/encoder/x86/itx_avx2.asm",
        "src/encoder/x86/itx_avx512.asm",
        "src/encoder/x86/itx_sse.asm",
        "src/encoder/x86/itx16_avx2.asm",
        "src/encoder/x86/itx16_avx512.asm",
        "src/encoder/x86/itx16_sse.asm",
        "src/encoder/x86/looprestoration_avx2.asm",
        "src/encoder/x86/looprestoration_avx512.asm",
        "src/encoder/x86/looprestoration_sse.asm",
        "src/encoder/x86/looprestoration16_avx2.asm",
        "src/encoder/x86/looprestoration16_avx512.asm",
        "src/encoder/x86/looprestoration16_sse.asm",
        "src/encoder/x86/mc_avx2.asm",
        "src/encoder/x86/mc_avx512.asm",
        "src/encoder/x86/mc_sse.asm",
        "src/encoder/x86/mc16_avx2.asm",
        "src/encoder/x86/mc16_avx512.asm",
        "src/encoder/x86/mc16_sse.asm",
        "src/encoder/x86/me.asm",
        "src/encoder/x86/sad_avx.asm",
        "src/encoder/x86/sad_sse2.asm",
        "src/encoder/x86/satd.asm",
        "src/encoder/x86/satd16_avx2.asm",
        "src/encoder/x86/sse.asm",
        "src/encoder/x86/tables.asm",
    ];

    let obj = nasm_rs::Build::new().out_dir(out_dir)
      .min_version(2, 15, 0)
      .include(&out_dir)
      .include("src/encoder")
      .files(asm_files)
      .compile_objects()
      .unwrap_or_else(|e| {
        panic!("NASM build failed. Make sure you have nasm installed or disable the \"asm\" feature.\n\
                You can get NASM from https://nasm.us or your system's package manager.\n\
                \n\
                error: {e}");
    });

    // cc is better at finding the correct archiver
    let mut cc = cc::Build::new();
    cc.out_dir(out_dir);
    for o in obj {
        cc.object(o);
    }
    cc.compile("rav1easm");

    // Strip local symbols from the asm library since they
    // confuse the debugger.
    if let Some(strip) = strip_command() {
        let _ = std::process::Command::new(strip)
            .arg("-x")
            .arg(Path::new(&out_dir).join("librav1easm.a"))
            .status();
    }
}

fn strip_command() -> Option<String> {
    let target = env::var("TARGET").expect("TARGET");
    // follows Cargo's naming convention for the linker setting
    let normalized_target = target.replace('-', "_").to_uppercase();
    let explicit_strip = env::var(format!("CARGO_TARGET_{normalized_target}_STRIP"))
        .ok()
        .or_else(|| env::var("STRIP").ok());
    if explicit_strip.is_some() {
        return explicit_strip;
    }

    // strip command is target-specific, e.g. macOS's strip breaks MUSL's archives
    let host = env::var("HOST").expect("HOST");
    if host != target {
        return None;
    }

    Some("strip".into())
}

pub(super) fn build() {
    if env::var("CARGO_CFG_TARGET_ARCH").unwrap() == "x86_64" {
        println!("cargo:rustc-cfg=nasm_x86_64");
        build_nasm_files();
    }
}
