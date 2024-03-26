#![allow(non_snake_case)]

use {
    crate::{
        encryption::elgamal::ElGamalPubkey as DecodedElGamalPubkey,
        zk_token_elgamal::pod::{wasm::display_to_jsvalue, ElGamalPubkey, Pod, Zeroable},
    },
    js_sys::{Array, Uint8Array},
    wasm_bindgen::{prelude::*, JsCast},
};

#[wasm_bindgen]
#[derive(Clone, Copy, Default, Pod, Zeroable, PartialEq, Eq)]
#[repr(transparent)]
pub struct PodElGamalPubkey(ElGamalPubkey);

#[wasm_bindgen]
impl PodElGamalPubkey {
    /// Create a new `ElGamalPubkey` object
    ///
    /// * `value` - optional public key as a base64 encoded string, `Uint8Array`, `[number]`
    #[wasm_bindgen(constructor)]
    pub fn constructor(value: JsValue) -> Result<PodElGamalPubkey, JsValue> {
        if let Some(base64_str) = value.as_string() {
            base64_str
                .parse::<ElGamalPubkey>()
                .map_err(display_to_jsvalue)
                .map(PodElGamalPubkey)
        } else if let Some(uint8_array) = value.dyn_ref::<Uint8Array>() {
            bytemuck::try_from_bytes(&uint8_array.to_vec())
                .map_err(|err| JsValue::from(format!("Invalid Uint8Array ElGamalPubkey: {err:?}")))
                .map(|pubkey| PodElGamalPubkey(*pubkey))
        } else if let Some(array) = value.dyn_ref::<Array>() {
            let mut bytes = vec![];
            let iterator = js_sys::try_iter(&array.values())?.expect("array to be iterable");
            for x in iterator {
                let x = x?;

                if let Some(n) = x.as_f64() {
                    if (0. ..=255.).contains(&n) {
                        bytes.push(n as u8);
                        continue;
                    }
                }
                return Err(format!("Invalid array argument: {:?}", x).into());
            }

            bytemuck::try_from_bytes(&bytes)
                .map_err(|err| JsValue::from(format!("Invalid Array pubkey: {err:?}")))
                .map(|pubkey| PodElGamalPubkey(*pubkey))
        } else if value.is_undefined() {
            Ok(PodElGamalPubkey::default())
        } else {
            Err("Unsupported argument".into())
        }
    }

    /// Return the base64 string representation of the public key
    pub fn toString(&self) -> String {
        self.0.to_string()
    }

    /// Checks if two `ElGamalPubkey`s are equal
    pub fn equals(&self, other: &PodElGamalPubkey) -> bool {
        self == other
    }

    /// Return the `Uint8Array` representation of the public key
    pub fn toBytes(&self) -> Box<[u8]> {
        self.0 .0.into()
    }

    pub fn encoded(decoded: &DecodedElGamalPubkey) -> Self {
        Self((*decoded).into())
    }

    pub fn decoded(&self) -> Result<DecodedElGamalPubkey, JsValue> {
        self.0
            .try_into()
            .map_err(|err| JsValue::from(format!("Invalid ElGamalPubkey: {err:?}")))
    }
}
