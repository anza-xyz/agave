use wasm_bindgen::prelude::*;

pub fn display_to_jsvalue<T: std::fmt::Display>(display: T) -> JsValue {
    display.to_string().into()
}
