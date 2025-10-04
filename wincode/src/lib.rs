//! wincode is a fast, bincode‑compatible serializer/deserializer focused on in‑place
//! initialization and direct memory writes.
//!
//! In short, `wincode` operates over traits that facilitate direct writes of memory
//! into final destinations (including heap-allocated buffers) without intermediate
//! staging buffers.
//!
//! # Quickstart
//!
//! `wincode` traits are implemented for many built-in types (like `Vec`, integers, etc.).
//!
//! You'll most likely want to start by using `wincode` on your own struct types, which can be
//! done with the [`compound!`] macro.
//!
//! ```
//! # use {wincode::compound, serde::{Serialize, Deserialize}};
//! # #[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
//! struct MyStruct {
//!     data: Vec<u64>,
//!     win: bool,
//! }
//!
//! compound! {
//!     MyStruct {
//!         data: Vec<u64>,
//!         win: bool,
//!     }
//! }
//!
//! let val = MyStruct { data: vec![1,2,3], win: true };
//! assert_eq!(wincode::serialize(&val).unwrap(), bincode::serialize(&val).unwrap());
//! ```
//!
//! For POD‑like fields (bytes, arrays of bytes, POD newtypes), use [`containers`]
//! to leverage optimized read/write implementations.
//!
//! ```
//! # use {wincode::{compound, containers::{self, Pod}}, serde::{Serialize, Deserialize}};
//! # #[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug)]
//! struct MyStruct {
//!     data: Vec<u8>,
//!     addresses: Vec<[u8; 32]>,
//! }
//!
//! compound! {
//!     MyStruct {
//!         data: containers::Vec<Pod<u8>>,
//!         addresses: containers::Vec<Pod<[u8; 32]>>,
//!     }
//! }
//!
//! let val = MyStruct { data: vec![1,2,3], addresses: vec![[0; 32], [1; 32]] };
//! assert_eq!(wincode::serialize(&val).unwrap(), bincode::serialize(&val).unwrap());
//! ```
//!
//! # Motivation
//!
//! Safe Rust offers limited support for placement initialization, especially for heap
//! memory. The effect of this is a lot of unnecessary copying, which is unacceptable in
//! performance‑critical code.
//! `serde` inherits these limitations since it neither attempts to initialize in‑place
//! nor exposes APIs to do so. `wincode` addresses this by providing schemas that support direct,
//! in‑place reads and writes for common patterns like byte buffers and POD types.
//!
//! # Adapting foreign types
//!
//! Another motivating feature of `wincode` is the ability to implement serialization/deserialization
//! on foreign types, where serialization/deserialization schemes on those types are unoptimized (and
//! out of your control as a foreign type). For example, suppose the following struct,
//! defined outside of your crate:
//! ```
//! use serde::{Serialize, Deserialize};
//!
//! #[derive(Serialize, Deserialize)]
//! pub struct A {
//!     pub data: Vec<u8>,
//!     pub address: [u8; 32],
//! }
//! ```
//!
//! `serde`'s default, naive, implementation will perform per-element visitation of all bytes
//! in `Vec<u8>` and `[u8; 32]`. Because these fields are "plain old data", ideally we would
//! avoid per-element visitation entirely and read / write these fields in a single pass.
//! The situation worsens if this struct needs to be written into a heap allocated data structure,
//! like a `Vec<A>` or `Box<[A]>`. Due to lack of placement initialization of heap memory, all
//! those bytes will be put on the stack before being copied into the heap allocation.
//!
//! `wincode` can solve this with the following:
//! ```
//! # use wincode::{compound, Serialize, Deserialize, containers::{self, Pod}};
//! # #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Eq)]
//! pub struct A {
//!     pub data: Vec<u8>,
//!     pub address: [u8; 32],
//! }
//!
//! compound! {
//!     struct MyA => A {
//!         data: containers::Vec<Pod<u8>>,
//!         address: Pod<[u8; 32]>,
//!     }
//! }
//!
//! let val = A {
//!     data: vec![1, 2, 3],
//!     address: [0; 32],
//! };
//! let bincode_serialize = bincode::serialize(&val).unwrap();
//! let wincode_serialize = MyA::serialize(&val).unwrap();
//! assert_eq!(bincode_serialize, wincode_serialize);
//!
//! let bincode_deserialize: A = bincode::deserialize(&bincode_serialize).unwrap();
//! let wincode_deserialize = MyA::deserialize(&bincode_serialize).unwrap();
//! assert_eq!(val, bincode_deserialize);
//! assert_eq!(val, wincode_deserialize);
//! ```
//!
//! Now, when deserializing `A`:
//! - All initialization is done in-place, including heap-allocated memory
//!   (true of all supported heap-allocated structures in `wincode`).
//! - Byte fields are written in a single pass by leveraging the
//!   [`Pod`](containers::Pod) in the [`containers`] module.
//! - No intermediate staging buffers: bytes are copied directly into the final
//!   destination allocation during deserialization and written directly into the
//!   output buffer during serialization.
//!
//! # Compatibility
//!
//! - Produces the same bytes as `bincode` for the covered shapes when using bincode's
//!   default configuration, provided your [`compound!`] schema and [`containers`] match the
//!   layout implied by your `serde` types.
//! - Length encodings are pluggable via [`SeqLen`](len::SeqLen).
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]
#[cfg(feature = "alloc")]
extern crate alloc;

pub mod error;
pub use error::{Error, Result};
pub mod io;
pub mod len;
mod schema;
pub use schema::*;
mod serde;
pub use serde::*;

#[doc(hidden)]
pub mod __private {
    #[doc(hidden)]
    pub use paste::paste;
}
