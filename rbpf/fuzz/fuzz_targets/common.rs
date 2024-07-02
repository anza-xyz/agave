use {
    arbitrary::{Arbitrary, Unstructured},
    solana_rbpf::vm::Config,
    std::mem::size_of,
};

#[derive(Debug)]
pub struct ConfigTemplate {
    max_call_depth: usize,
    instruction_meter_checkpoint_distance: usize,
    noop_instruction_rate: u32,
    enable_stack_frame_gaps: bool,
    enable_symbol_and_section_labels: bool,
    sanitize_user_provided_values: bool,
    reject_callx_r10: bool,
    optimize_rodata: bool,
}

impl<'a> Arbitrary<'a> for ConfigTemplate {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let bools = u16::arbitrary(u)?;
        Ok(ConfigTemplate {
            max_call_depth: usize::from(u8::arbitrary(u)?).saturating_add(1), // larger is unreasonable + must be non-zero
            instruction_meter_checkpoint_distance: usize::from(u16::arbitrary(u)?), // larger is unreasonable
            noop_instruction_rate: u32::from(u16::arbitrary(u)?),
            enable_stack_frame_gaps: bools & (1 << 0) != 0,
            enable_symbol_and_section_labels: bools & (1 << 1) != 0,
            sanitize_user_provided_values: bools & (1 << 3) != 0,
            reject_callx_r10: bools & (1 << 6) != 0,
            optimize_rodata: bools & (1 << 9) != 0,
        })
    }

    fn size_hint(_: usize) -> (usize, Option<usize>) {
        (
            size_of::<u8>()
                .saturating_add(size_of::<u16>())
                .saturating_add(size_of::<f64>())
                .saturating_add(size_of::<u16>()),
            None,
        )
    }
}

impl From<ConfigTemplate> for Config {
    fn from(template: ConfigTemplate) -> Self {
        let ConfigTemplate {
            max_call_depth,
            instruction_meter_checkpoint_distance,
            noop_instruction_rate,
            enable_stack_frame_gaps,
            enable_symbol_and_section_labels,
            sanitize_user_provided_values,
            reject_callx_r10,
            optimize_rodata,
        } = template;

        Config {
            max_call_depth,
            enable_stack_frame_gaps,
            instruction_meter_checkpoint_distance,
            enable_symbol_and_section_labels,
            noop_instruction_rate,
            sanitize_user_provided_values,
            reject_callx_r10,
            optimize_rodata,
            ..Default::default()
        }
    }
}
