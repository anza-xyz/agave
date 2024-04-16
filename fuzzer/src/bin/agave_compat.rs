use clap::Parser;
use prost::Message;
use std::path::PathBuf;
use agave_fuzzer::proto::InstrContext;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    inputs: Vec<PathBuf>,
}

fn exec(input: &PathBuf) {
    let blob = std::fs::read(input).unwrap();
    let context = InstrContext::decode(&blob[..]).unwrap();
    let effects = match agave_fuzzer::execute_instr_proto(context) {
        Some(e) => e,
        None => {
            println!("No instruction effects returned.");
            return;
        }
    };
    eprintln!("Effects: {:?}", effects);
}

fn main() {
    let cli = Cli::parse();
    for input in cli.inputs {
        exec(&input);
    }
}