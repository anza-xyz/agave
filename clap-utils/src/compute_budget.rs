use {
    crate::{input_validators::is_parsable, ArgConstant},
    clap::Arg,
};

pub const COMPUTE_UNIT_PRICE_ARG: ArgConstant<'static> = ArgConstant {
    name: "compute_unit_price",
    long: "--with-compute-unit-price",
    help: "Set compute unit price for transaction, in increments of 0.000001 lamports per compute unit.",
};

pub fn compute_unit_price_arg<'a, 'b>() -> Arg<'a, 'b> {
    Arg::with_name(COMPUTE_UNIT_PRICE_ARG.name)
        .long(COMPUTE_UNIT_PRICE_ARG.long)
        .takes_value(true)
        .value_name("COMPUTE-UNIT-PRICE")
        .validator(is_parsable::<u64>)
        .help(COMPUTE_UNIT_PRICE_ARG.help)
}
