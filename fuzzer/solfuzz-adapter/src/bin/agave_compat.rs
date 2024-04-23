use {clap::Parser, prost::Message, solfuzz_adapter::proto::InstrContext, std::path::PathBuf};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    inputs: Vec<PathBuf>,
}

fn exec(input: &PathBuf) {
    let blob = std::fs::read(input).unwrap();
    let context = InstrContext::decode(&blob[..]).unwrap();
    let Some(effects) = solfuzz_adapter::execute_instr_proto(context) else {
        println!("No instruction effects returned.");
        return;
    };
    eprintln!("Effects: {:?}", effects);
}

fn main() {
    let cli = Cli::parse();
    for input in cli.inputs {
        exec(&input);
    }
}
