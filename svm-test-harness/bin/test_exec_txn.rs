use {
    clap::Parser,
    prost::Message,
    solana_svm_test_harness::{
        fixture::proto::TxnFixture as ProtoTxnFixture, fuzz::execute_txn_proto,
    },
    std::path::PathBuf,
};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    inputs: Vec<PathBuf>,
}

fn exec(input: &PathBuf) -> bool {
    let blob = std::fs::read(input).unwrap();
    let fixture = ProtoTxnFixture::decode(&blob[..]).unwrap();
    let Some(context) = fixture.input else {
        println!("No context found.");
        return false;
    };

    let Some(expected) = fixture.output else {
        println!("No fixture found.");
        return false;
    };
    let Some(result) = execute_txn_proto(context) else {
        println!("FAIL: No transaction result returned for input: {input:?}",);
        return false;
    };

    let ok = result == expected;

    if ok {
        println!("OK: {input:?}");
    } else {
        println!("FAIL: {input:?}");
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
