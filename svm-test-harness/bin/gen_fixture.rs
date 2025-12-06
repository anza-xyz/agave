use clap::Parser;
use prost::Message;
use solana_svm_test_harness::{
    elf_loader::execute_elf_loader,
    fixture::proto::{
        ElfBinary, ElfLoaderCtx, ElfLoaderEffects, ElfLoaderFixture, FeatureSet,
        FixtureMetadata,
    },
};
use std::{fs, path::PathBuf};

#[derive(Parser, Debug)]
#[command(name = "gen_fixture")]
#[command(about = "Generate test fixture files")]
struct Args {
    /// Test suite (e.g., elf_loader, instr)
    #[arg(long)]
    suite: String,

    /// Output directory for the generated fixture
    #[arg(long)]
    out_dir: Option<PathBuf>,

    /// Output filename (e.g. smoke_invalid.pb)
    #[arg(long)]
    name: Option<String>,

    // ELF Loader specific arguments
    /// Path to ELF file to embed (elf_loader suite only)
    #[arg(long)]
    elf: Option<PathBuf>,

    /// Set deploy_checks flag (elf_loader suite only)
    #[arg(long, default_value_t = false)]
    deploy_checks: bool,
}

enum Suite {
    ElfLoader,
    // Future: Instr,
}

impl Suite {
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "elf_loader" => Ok(Suite::ElfLoader),
            // "instr" => Ok(Suite::Instr),
            _ => Err(format!("Unknown suite: {}", s)),
        }
    }

    fn default_out_dir(&self) -> &str {
        match self {
            Suite::ElfLoader => "svm-test-harness/test-vectors/elf_loader",
        }
    }

    fn default_filename(&self) -> &str {
        match self {
            Suite::ElfLoader => "fixture.pb",
        }
    }
}

fn generate_elf_loader_fixture(args: &Args) -> Result<Vec<u8>, String> {
    // Read ELF bytes or use default invalid bytes
    let elf_bytes = match &args.elf {
        Some(path) => fs::read(path)
            .map_err(|e| format!("Failed to read ELF file '{}': {}", path.display(), e))?,
        None => vec![0x00, 0x01, 0x02, 0x03], // deliberately invalid for smoke tests
    };

    // Create context
    let ctx = ElfLoaderCtx {
        elf: Some(ElfBinary { data: elf_bytes }),
        features: Some(FeatureSet { features: vec![] }),
        deploy_checks: args.deploy_checks,
    };

    // Execute to generate effects
    let effects: ElfLoaderEffects = execute_elf_loader(&ctx).unwrap_or_default();

    // Build fixture
    let fixture = ElfLoaderFixture {
        metadata: Some(FixtureMetadata {
            fn_entrypoint: "elf_loader".to_string(),
        }),
        input: Some(ctx),
        output: Some(effects),
    };

    Ok(fixture.encode_to_vec())
}

fn generate_fixture(suite: &Suite, args: &Args) -> Result<Vec<u8>, String> {
    match suite {
        Suite::ElfLoader => generate_elf_loader_fixture(args),
    }
}

fn main() {
    let args = Args::parse();

    // Parse suite
    let suite = match Suite::from_str(&args.suite) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", e);
            eprintln!("\nSupported suites:");
            eprintln!("  elf_loader - ELF loader test fixtures");
            std::process::exit(2);
        }
    };

    // Determine output directory and filename
    let out_dir = args
        .out_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(suite.default_out_dir()));

    let filename = args
        .name
        .clone()
        .unwrap_or_else(|| suite.default_filename().to_string());

    // Generate fixture
    let bytes = match generate_fixture(&suite, &args) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Error generating fixture: {}", e);
            std::process::exit(1);
        }
    };

    // Write output
    if let Err(e) = fs::create_dir_all(&out_dir) {
        eprintln!("Failed to create output directory: {}", e);
        std::process::exit(1);
    }

    let out_path = out_dir.join(filename);
    if let Err(e) = fs::write(&out_path, bytes) {
        eprintln!("Failed to write fixture: {}", e);
        std::process::exit(1);
    }

    println!("Generated fixture: {}", out_path.display());
}

