use {clap::Parser, prost::Message, protosol::protos::GossipFixture, std::path::PathBuf};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    inputs: Vec<PathBuf>,
}

fn exec(input: &PathBuf) -> bool {
    let blob = std::fs::read(input).unwrap();
    let Ok(fixture) = GossipFixture::decode(&blob[..]) else {
        println!("Failed to parse fixture.");
        return false;
    };

    let Some(expected) = fixture.output else {
        println!("No fixture found.");
        return false;
    };

    let effects = agave_conformance::gossip::gossip_decode_to_effects(&fixture.input);

    let ok = effects == expected;
    if ok {
        println!("OK: {input:?}");
    } else {
        println!("FAIL: {input:?}");
        println!("Expected: {expected:?}");
        println!("Actual: {effects:?}");
    }
    ok
}

fn main() {
    let cli = Cli::parse();
    let mut fail_cnt: i32 = 0;
    for input in cli.inputs {
        if !exec(&input) {
            fail_cnt = fail_cnt.saturating_add(1);
        }
    }
    std::process::exit(fail_cnt);
}
