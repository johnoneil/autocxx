// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use autocxx::prelude::*;
include_cpp! {
    #include "input.h"
    safety!(unsafe_ffi)

    // Just some basic structs used by our API. None problematic.
    generate!("my_namespace::X")
    generate!("my_namespace::Y")
    generate!("my_namespace::Z")
    generate!("my_namespace::Rect")

    // Our primary class of interest. This defines the API we're using.
    generate!("my_namespace::MyPrimaryClass")

    // Being able to do this would help with this issue I think (or something analogous).
    //block!("my_namespace::MyPrimaryClass::method_broken")

    // One particular static method fails due to some obscure C++ construction
    // These help reproduce this case.
    //generate!("my_namespace::MyProblematicClass")
    //generate!("my_namespace::make_variant")
}

fn main() {
    // Call some methods on the primary class of interest
    let mut primary_class = ffi::my_namespace::MyPrimaryClass::new().within_unique_ptr();
    let result_one = primary_class.pin_mut().method_one();
    println!("PrimaryClass method_one returns: {}", result_one);
    let result_two = primary_class.pin_mut().method_two();
    println!("PrimaryClass method_two returns: {}", result_two);

    // Call a problematic static method on the primary class of interest
    //let my = ffi::my_namespace::MyClass::new().within_box();
    //let variant = ffi::my_namespace::make_variant();
    //println!("PrimaryClass method_one returns: {}", my_namespace::PrimaryClass::method_one());
}
