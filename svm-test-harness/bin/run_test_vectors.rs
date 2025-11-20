use clap::Parser;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{fs, io};

#[derive(Parser, Debug)]
#[command(name = "run_test_vectors")]
#[command(about = "Run SVM test vectors")]
struct Args {
    /// Test suite to run (e.g., elf_loader)
    #[arg(long)]
    suite: String,

    /// Optional: specific test case (fixture name or path)
    #[arg(long)]
    case: Option<String>,

    /// Print verbose output
    #[arg(long)]
    verbose: bool,
}

enum Suite {
    ElfLoader,
}

impl Suite {
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "elf_loader" => Ok(Suite::ElfLoader),
            _ => Err(format!("Unknown suite: {}", s)),
        }
    }

    fn dir(&self) -> &str {
        match self {
            Suite::ElfLoader => "svm-test-harness/test-vectors/elf_loader",
        }
    }

    fn test_binary(&self) -> &str {
        match self {
            Suite::ElfLoader => "test_exec_elf_loader",
        }
    }
}

/// Resolve a fixture path from a name, path, or substring match
fn resolve_fixture_path(suite: &Suite, name: &str, root: &Path) -> Result<PathBuf, String> {
    let dir = root.join(suite.dir());
    
    // Check if name is an absolute or relative path that exists
    let name_path = Path::new(name);
    if name_path.is_file() {
        return Ok(name_path.to_path_buf());
    }

    // Check exact filename in suite dir
    for ext in &["", ".pb", ".fix"] {
        let path = dir.join(format!("{}{}", name, ext));
        if path.is_file() {
            return Ok(path);
        }
    }

    // Find unique substring match against basenames
    let mut matches = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension() {
                    if ext == "pb" || ext == "fix" {
                        if let Some(basename) = path.file_name().and_then(|n| n.to_str()) {
                            if basename.contains(name) {
                                matches.push(path);
                            }
                        }
                    }
                }
            }
        }
    }

    match matches.len() {
        0 => Err(format!(
            "Fixture '{}' not found in {}",
            name,
            dir.display()
        )),
        1 => Ok(matches.into_iter().next().unwrap()),
        _ => {
            let mut msg = format!("Ambiguous fixture name '{}'. Matches:\n", name);
            for m in matches {
                msg.push_str(&format!("  {}\n", m.display()));
            }
            Err(msg)
        }
    }
}

/// Find all fixture files in a directory
fn find_fixtures(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut fixtures = Vec::new();
    
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension() {
                    if ext == "pb" || ext == "fix" {
                        fixtures.push(path);
                    }
                }
            }
        }
    }
    
    fixtures.sort();
    Ok(fixtures)
}

struct TestResults {
    total: usize,
    passed: usize,
    failed: usize,
}

impl TestResults {
    fn new() -> Self {
        Self {
            total: 0,
            passed: 0,
            failed: 0,
        }
    }

    fn print_summary(&self) {
        println!(
            "Summary: total={}, passed={}, failed={}",
            self.total, self.passed, self.failed
        );
    }
}

/// Run test vectors for a suite
fn run_suite(suite: &Suite, case: Option<&str>, verbose: bool, root: &Path) -> Result<(), String> {
    let dir = root.join(suite.dir());
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create directory: {}", e))?;

    // Collect fixture files
    let files = if let Some(case_name) = case {
        vec![resolve_fixture_path(suite, case_name, root)?]
    } else {
        find_fixtures(&dir).map_err(|e| format!("Failed to find fixtures: {}", e))?
    };

    if files.is_empty() {
        return Err(format!("No test fixtures found in {}", dir.display()));
    }

    let mut results = TestResults::new();

    for file in &files {
        if verbose {
            println!("Running {}", file.display());
        }

        results.total += 1;

        // Run: cargo run -q -p solana-svm-test-harness --features fuzz --bin <binary> -- <file>
        let status = Command::new("cargo")
            .arg("run")
            .arg("-q")
            .arg("-p")
            .arg("solana-svm-test-harness")
            .arg("--features")
            .arg("fuzz")
            .arg("--bin")
            .arg(suite.test_binary())
            .arg("--")
            .arg(file)
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| format!("Failed to execute cargo: {}", e))?;

        if status.success() {
            results.passed += 1;
            println!("PASS: {}", file.display());
        } else {
            results.failed += 1;
            println!("FAIL: {}", file.display());
            results.print_summary();
            return Err(format!("Test failed: {}", file.display()));
        }
    }

    results.print_summary();
    Ok(())
}

fn main() {
    let args = Args::parse();

    // Find repository root (go up from the binary location until we find Cargo.toml)
    let root = std::env::current_dir()
        .expect("Failed to get current directory");

    let suite = match Suite::from_str(&args.suite) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(2);
        }
    };

    if let Err(e) = run_suite(&suite, args.case.as_deref(), args.verbose, &root) {
        eprintln!("{}", e);
        std::process::exit(1);
    }
}
