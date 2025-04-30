use std::process::Command;

fn main() {
    if let Ok(git_output) = Command::new("git").args(["rev-parse", "HEAD"]).output() {
        if git_output.status.success() {
            if let Ok(git_commit_hash) = String::from_utf8(git_output.stdout) {
                let trimmed_hash = git_commit_hash.trim().to_string();
                println!("cargo:rustc-env=AGAVE_GIT_COMMIT_HASH={}", trimmed_hash);
            }
        }
    }
}
