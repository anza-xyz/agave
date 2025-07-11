use std::env;
use std::fs;
use std::path::Path;
use std::sync::Arc;

fn main() {
    // Only rebuild if the emergency abort source changes
    println!("cargo:rerun-if-changed=emergency-abort/src/sbpf-asm-abort.s");
    println!("cargo:rerun-if-changed=emergency-abort/src/sbpf-asm-abort.ld");
    
    build_emergency_abort_program();
}

fn build_emergency_abort_program() {
    let out_dir = env::var("OUT_DIR").unwrap();
    let out_path = Path::new(&out_dir);
    
    // Read the assembly source directly
    let assembly_source = fs::read_to_string("emergency-abort/src/sbpf-asm-abort.s")
        .expect("Failed to read emergency abort assembly source");
    
    println!("cargo:warning=Assembly source: {}", assembly_source.trim());
    
    // Use solana-sbpf assembler to compile the assembly source
    let binary_data = match assemble_emergency_abort(&assembly_source) {
        Ok(data) => {
            println!("cargo:warning=Successfully assembled emergency abort program ({} bytes)", data.len());
            data
        }
        Err(e) => {
            panic!("Failed to assemble emergency abort program from source: {}\n\
                   Assembly source:\n{}\n\
                   This ensures binary data is compiled from transparent source code, not hardcoded.", 
                   e, assembly_source);
        }
    };
    
    // Generate the Rust code with the binary data
    let mut output = String::new();
    output.push_str("// Emergency abort program binary (assembled from .s source)\n");
    output.push_str("// Based on: https://github.com/deanmlittle/sbpf-asm-abort\n");
    output.push_str("// Author: deanmlittle\n");
    output.push_str("// Assembly source: .globl e\\ne:\\n  lddw r0, 1\\n  exit\n");
    output.push_str(&format!("// Size: {} bytes\n", binary_data.len()));
    output.push_str("// SECURITY: This binary was assembled from .s source using solana-sbpf\n");
    output.push_str(&format!("pub const EMERGENCY_ABORT_PROGRAM: &[u8] = &{:?};\n", binary_data));
    
    // Write the generated code
    let generated_file = out_path.join("emergency_abort.rs");
    fs::write(&generated_file, output)
        .expect("Failed to write emergency abort code");
    
    // Tell cargo about the generated file location
    println!("cargo:rustc-env=EMERGENCY_ABORT_RS={}", generated_file.display());
}

fn assemble_emergency_abort(assembly_source: &str) -> Result<Vec<u8>, String> {
    use solana_sbpf::{
        assembler::assemble,
        verifier::RequisiteVerifier,
    };
    
    // Create a program runtime environment like in ledger-tool
    let program_runtime_environment = 
                 solana_bpf_loader_program::syscalls::create_program_runtime_environment_v1(
             &solana_svm_feature_set::SVMFeatureSet::default(),
             &solana_program_runtime::execution_budget::SVMTransactionExecutionBudget::default(),
            false, // reject_deployment_of_broken_elfs 
            false, // debugging_features
        ).map_err(|e| format!("Failed to create runtime environment: {:?}", e))?;
    
    // Assemble the source code like in ledger-tool  
    let executable = assemble::<solana_program_runtime::invoke_context::InvokeContext>(
        assembly_source,
        Arc::new(program_runtime_environment),
    ).map_err(|e| format!("Assembly failed: {:?}", e))?;
    
    // Verify the executable
    executable.verify::<RequisiteVerifier>()
        .map_err(|e| format!("Verification failed: {:?}", e))?;
    
    // Extract the ELF bytes  
    let elf_bytes = executable.get_text_bytes().1;
    Ok(elf_bytes.to_vec())
} 