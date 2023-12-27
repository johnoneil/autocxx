// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

fn main() -> miette::Result<()> {
    let path = std::path::PathBuf::from("src");
    let mut autocxx_builder = autocxx_build::Builder::new("src/main.rs", [&path])
        .extra_clang_args(&["-std=c++17"])
        .build()?;
    //.expect_build();

    autocxx_builder
        .compiler("clang++")
        .flag_if_supported("-std=c++17")
        .compile("namespace-example");

    // .compiler(cxx)
    // .flag_if_supported("-std=c++17")
    // .flag_if_supported("-Wno-unused-parameter")

    println!("cargo:rerun-if-changed=src/main.rs");
    println!("cargo:rerun-if-changed=src/input.h");
    Ok(())
}
