use clap::Parser;
use prost::Message;
use solana_svm_test_harness::{
    elf_loader::execute_elf_loader,
    fixture::proto::ElfLoaderFixture,
};
use std::{fs, path::PathBuf};

#[derive(Parser, Debug)]
#[command(name = "update_fixtures")]
#[command(about = "Update test fixtures with current behavior")]
struct Args {
    /// Test suite (e.g., elf_loader)
    #[arg(long)]
    suite: String,

    /// Input directory containing fixtures
    #[arg(long)]
    input_dir: PathBuf,

    /// Update fixtures in place (otherwise just report)
    #[arg(long)]
    update: bool,

    /// Verbose output
    #[arg(long)]
    verbose: bool,
}

fn update_elf_loader_fixture(path: &PathBuf, update: bool, verbose: bool) -> Result<bool, String> {
    // Read fixture
    let data = fs::read(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let mut fixture = ElfLoaderFixture::decode(&data[..])
        .map_err(|e| format!("Failed to decode {}: {}", path.display(), e))?;

    let Some(ctx) = &fixture.input else {
        return Err(format!("No context in {}", path.display()));
    };

    let Some(old_output) = &fixture.output else {
        return Err(format!("No output in {}", path.display()));
    };

    // Execute with current implementation
    let new_output = execute_elf_loader(ctx).unwrap_or_default();

    // Check if output changed
    if new_output == *old_output {
        if verbose {
            println!("  OK: {}", path.display());
        }
        return Ok(false);
    }

    // Output changed - needs update
    println!("  OUTDATED: {}", path.display());
    
    if old_output.error != new_output.error {
        println!("  Error code changed: {} -> {}", old_output.error, new_output.error);
    }
    
    if old_output.rodata_sz != new_output.rodata_sz {
        println!("  Rodata size changed: {} -> {}", old_output.rodata_sz, new_output.rodata_sz);
    }
    
    if old_output.text_cnt != new_output.text_cnt {
        println!("  Text count changed: {} -> {}", old_output.text_cnt, new_output.text_cnt);
    }
    
    if old_output.calldests.len() != new_output.calldests.len() {
        println!(
            "  Calldests count changed: {} -> {}",
            old_output.calldests.len(),
            new_output.calldests.len()
        );
    }

    if update {
        // Update the fixture
        fixture.output = Some(new_output);
        let updated_bytes = fixture.encode_to_vec();
        
        fs::write(path, updated_bytes)
            .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;
        
        println!("  Updated!");
    } else {
        println!("  (use --update to apply changes)");
    }

    Ok(true)
}

fn main() {
    let args = Args::parse();

    if args.suite != "elf_loader" {
        eprintln!("Error: Only 'elf_loader' suite is currently supported");
        std::process::exit(2);
    }

    // Find all fixtures
    let Ok(entries) = fs::read_dir(&args.input_dir) else {
        eprintln!("Failed to read directory: {}", args.input_dir.display());
        std::process::exit(1);
    };

    let mut fixtures = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_file() {
            if let Some(ext) = path.extension() {
                if ext == "fix" || ext == "pb" {
                    fixtures.push(path);
                }
            }
        }
    }

    fixtures.sort();

    println!("Checking {} fixtures...\n", fixtures.len());

    let mut updated_count = 0;
    let mut ok_count = 0;
    let mut error_count = 0;

    for path in &fixtures {
        match update_elf_loader_fixture(path, args.update, args.verbose) {
            Ok(true) => updated_count += 1,
            Ok(false) => ok_count += 1,
            Err(e) => {
                eprintln!("  ERROR: {}", e);
                error_count += 1;
            }
        }
    }

    println!("\n═══════════════════════════════");
    println!("Summary:");
    println!("  Total:    {}", fixtures.len());
    println!("  OK:       {}", ok_count);
    println!("  Outdated: {}", updated_count);
    println!("  Errors:   {}", error_count);
    
    if updated_count > 0 && !args.update {
        println!("\nRun with --update to apply changes");
    }

    if error_count > 0 {
        std::process::exit(1);
    }
}

