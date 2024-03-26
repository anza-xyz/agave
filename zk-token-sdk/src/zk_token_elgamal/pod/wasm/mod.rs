//! zk-token-sdk Javascript interface
use wasm_bindgen::prelude::*;

pub mod elgamal;

pub fn display_to_jsvalue<T: std::fmt::Display>(display: T) -> JsValue {
    display.to_string().into()
}
