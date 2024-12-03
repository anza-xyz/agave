//! The `Instructions` struct is a legacy workaround
//! from when wasm-bindgen lacked Vec<T> support
//! (ref: https://github.com/rustwasm/wasm-bindgen/issues/111)
use {crate::Instruction, wasm_bindgen::prelude::*};

#[wasm_bindgen]
#[derive(Default)]
pub struct Instructions {
    instructions: std::vec::Vec<Instruction>,
}

#[wasm_bindgen]
impl Instructions {
    #[wasm_bindgen(constructor)]
    pub fn constructor() -> Instructions {
        Instructions::default()
    }

    pub fn push(&mut self, instruction: Instruction) {
        self.instructions.push(instruction);
    }
}

impl From<Instructions> for std::vec::Vec<Instruction> {
    fn from(instructions: Instructions) -> Self {
        instructions.instructions
    }
}
