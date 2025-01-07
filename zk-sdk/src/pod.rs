use bytemuck_derive::{Pod, Zeroable};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(transparent)]
pub struct PodU16([u8; 2]);
impl From<u16> for PodU16 {
    fn from(n: u16) -> Self {
        Self(n.to_le_bytes())
    }
}
impl From<PodU16> for u16 {
    fn from(pod: PodU16) -> Self {
        Self::from_le_bytes(pod.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(transparent)]
pub struct PodU64([u8; 8]);
impl From<u64> for PodU64 {
    fn from(n: u64) -> Self {
        Self(n.to_le_bytes())
    }
}
impl From<PodU64> for u64 {
    fn from(pod: PodU64) -> Self {
        Self::from_le_bytes(pod.0)
    }
}

macro_rules! impl_from_str {
    (TYPE = $type:ident, BYTES_LEN = $bytes_len:expr, BASE64_LEN = $base64_len:expr) => {
        impl std::str::FromStr for $type {
            type Err = crate::errors::ParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                if s.len() > $base64_len {
                    return Err(Self::Err::WrongSize);
                }
                let mut bytes = [0u8; $bytes_len];
                let decoded_len = BASE64_STANDARD
                    .decode_slice(s, &mut bytes)
                    .map_err(|_| Self::Err::Invalid)?;
                if decoded_len != $bytes_len {
                    Err(Self::Err::WrongSize)
                } else {
                    Ok($type(bytes))
                }
            }
        }
    };
}
pub(crate) use impl_from_str;

macro_rules! impl_from_bytes {
    (TYPE = $type:ident, BYTES_LEN = $bytes_len:expr) => {
        impl std::convert::From<[u8; $bytes_len]> for $type {
            fn from(bytes: [u8; $bytes_len]) -> Self {
                Self(bytes)
            }
        }
    };
}
pub(crate) use impl_from_bytes;

macro_rules! impl_wasm_bindings {
    (POD_TYPE = $pod_type:ident, DECODED_TYPE = $decoded_type: ident) => {
        #[cfg(target_arch = "wasm32")]
        #[allow(non_snake_case)]
        #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
        impl $pod_type {
            #[wasm_bindgen::prelude::wasm_bindgen(constructor)]
            pub fn constructor(
                value: wasm_bindgen::JsValue,
            ) -> Result<$pod_type, wasm_bindgen::JsValue> {
                if let Some(base64_str) = value.as_string() {
                    base64_str
                        .parse::<$pod_type>()
                        .map_err(|e| e.to_string().into())
                } else if let Some(uint8_array) = value.dyn_ref::<js_sys::Uint8Array>() {
                    bytemuck::try_from_bytes(&uint8_array.to_vec())
                        .map_err(|err| {
                            wasm_bindgen::JsValue::from(format!("Invalid Uint8Array: {err:?}"))
                        })
                        .map(|value| *value)
                } else if let Some(array) = value.dyn_ref::<js_sys::Array>() {
                    let mut bytes = vec![];
                    let iterator =
                        js_sys::try_iter(&array.values())?.expect("array to be iterable");
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
                        .map_err(|err| {
                            wasm_bindgen::JsValue::from(format!("Invalid Array: {err:?}"))
                        })
                        .map(|value| *value)
                } else if value.is_undefined() {
                    Ok($pod_type::default())
                } else {
                    Err("Unsupported argument".into())
                }
            }

            pub fn toString(&self) -> String {
                self.to_string()
            }

            pub fn equals(&self, other: &$pod_type) -> bool {
                self == other
            }

            pub fn toBytes(&self) -> Box<[u8]> {
                self.0.into()
            }

            pub fn zeroed() -> Self {
                Self::default()
            }

            pub fn encode(decoded: &$decoded_type) -> $pod_type {
                (*decoded).into()
            }

            pub fn decode(&self) -> Result<$decoded_type, wasm_bindgen::JsValue> {
                (*self)
                    .try_into()
                    .map_err(|err| JsValue::from(format!("Invalid encoding: {err:?}")))
            }
        }
    };
}
pub(crate) use impl_wasm_bindings;
