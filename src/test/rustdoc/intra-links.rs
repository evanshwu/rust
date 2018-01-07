// Copyright 2017 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// @has intra_links/index.html
// @has - '//a/@href' '../intra_links/struct.ThisType.html'
// @has - '//a/@href' '../intra_links/enum.ThisEnum.html'
// @has - '//a/@href' '../intra_links/trait.ThisTrait.html'
// @has - '//a/@href' '../intra_links/type.ThisAlias.html'
// @has - '//a/@href' '../intra_links/union.ThisUnion.html'
// @has - '//a/@href' '../intra_links/fn.this_function.html'
// @has - '//a/@href' '../intra_links/constant.THIS_CONST.html'
// @has - '//a/@href' '../intra_links/static.THIS_STATIC.html'
// @has - '//a/@href' '../intra_links/macro.this_macro.html'
//! In this crate we would like to link to:
//!
//! * [`ThisType`](ThisType)
//! * [`ThisEnum`](ThisEnum)
//! * [`ThisTrait`](ThisTrait)
//! * [`ThisAlias`](ThisAlias)
//! * [`ThisUnion`](ThisUnion)
//! * [`this_function`](this_function)
//! * [`THIS_CONST`](THIS_CONST)
//! * [`THIS_STATIC`](THIS_STATIC)
//! * [`this_macro`](this_macro!)

#[macro_export]
macro_rules! this_macro {
    () => {};
}

pub struct ThisType;
pub enum ThisEnum { ThisVariant, }
pub trait ThisTrait {}
pub type ThisAlias = Result<(), ()>;
pub union ThisUnion { this_field: usize, }

pub fn this_function() {}
pub const THIS_CONST: usize = 5usize;
pub static THIS_STATIC: usize = 5usize;
