// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#pragma once

// #include <cstdint>
// #include <sstream>
// #include <stdint.h>
// #include <string>
#include <variant>
#include <memory>

namespace my_namespace {

struct X {};
struct Y {};
struct Z {};
struct Rect {};

// The problem arises when the Variant member of this class pollutes our API.
class MyProblematicClass {
    public:
    MyProblematicClass() {}

    using Variant = std::variant<std::shared_ptr<X>,
                               std::shared_ptr<Y>,
                               std::shared_ptr<Z>,
                               Rect>;
};

// this is just a helper in some cases to generate a Variant from the above case in rust.
inline MyProblematicClass::Variant make_variant() {
    MyProblematicClass::Variant variant;
    return variant;
}

// This is our primary class of interest.
// This represents our primary API.
class MyPrimaryClass {
public:
    uint32_t method_one();
    uint32_t method_two();

    // Problems arise when the ProblematicClass::Variant above starts to pollute the API.
    // I believe the "fix" is to be able to individually disable static methods on this class
    // via `block!` or other. Maybe there's another path.
    // But including this method currently causes incorrect C++ to be generated, which fails via:
    // error: redefinition of 'MyProblematicClass' as different kind of symbol
#if 1
    // Problem arises when obscure class above pollutes the API
    uint32_t method_broken(const MyProblematicClass::Variant& variant);
#endif
};

inline uint32_t MyPrimaryClass::method_one() {
    return 1;
}
inline uint32_t MyPrimaryClass::method_two() {
    return 2;
}

#if 1
inline uint32_t method_broken(const MyProblematicClass::Variant& variant) {
    return 3;
}
#endif

} // my_namespace
