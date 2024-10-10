use anyhow::anyhow;
use crossbeam_channel::{unbounded, Receiver};
use indicatif::{HumanDuration, ProgressBar, ProgressStyle};
use reqwest::Client;
use serde_json::json;
use solana_sdk::pubkey::Pubkey;
use std::thread;
use std::time::{Duration, Instant};

use crate::api::models::{
    ErrorResponse, JobResponse, JobStatus, JobVerificationResponse, VerifyResponse,
};

// URL for the remote server
pub const REMOTE_SERVER_URL: &str = "https://verify.osec.io";

fn loading_animation(receiver: Receiver<bool>) {
    let started = Instant::now();
    let spinner_style =
        ProgressStyle::with_template("[{elapsed_precise}] {prefix:.bold.dim} {spinner} {wide_msg}")
            .unwrap()
            .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");

    let pb = ProgressBar::new_spinner();
    pb.set_style(spinner_style);
    pb.set_message("Request sent. Awaiting server response. This may take a moment... ⏳");
    loop {
        match receiver.try_recv() {
            Ok(result) => {
                if result {
                    pb.finish_with_message(format!(
                        "✅ Process completed. (Done in {})\n",
                        HumanDuration(started.elapsed())
                    ));
                } else {
                    pb.finish_with_message("❌ Request processing failed.");
                    println!("❌ Time elapsed : {}", HumanDuration(started.elapsed()));
                }
                break;
            }

            Err(_) => {
                pb.inc(1);
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn print_verification_status(
    program_id: &str,
    status: bool,
    status_response: &JobVerificationResponse,
) {
    let status_message = if status {
        format!("Program {} has been verified. ✅", program_id)
    } else {
        format!("Program {} has not been verified. ❌", program_id)
    };
    let message = if status {
        "The provided GitHub build matches the on-chain hash."
    } else {
        "The provided GitHub build does not match the on-chain hash."
    };
    println!("{}", status_message);
    println!("{}", message);
    println!("On Chain Hash: {}", status_response.on_chain_hash.as_str());
    println!(
        "Executable Hash: {}",
        status_response.executable_hash.as_str()
    );
    println!("Repo URL: {}", status_response.repo_url.as_str());
}

// Send a job to the remote server
#[allow(clippy::too_many_arguments)]
pub async fn send_job_to_remote(
    repo_url: &str,
    commit_hash: &Option<String>,
    program_id: &Pubkey,
    library_name: &Option<String>,
    bpf_flag: bool,
    relative_mount_path: String,
    base_image: Option<String>,
    cargo_args: Vec<String>,
) -> anyhow::Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(18000))
        .build()?;

    // Send the POST request
    let response = client
        .post(format!("{}/verify", REMOTE_SERVER_URL))
        .json(&json!({
            "repository": repo_url,
            "commit_hash": commit_hash,
            "program_id": program_id.to_string(),
            "lib_name": library_name,
            "bpf_flag": bpf_flag,
            "mount_path":  if relative_mount_path.is_empty() {
                None
            } else {
                Some(relative_mount_path)
            },
            "base_image": base_image,
            "cargo_args": cargo_args,
        }))
        .send()
        .await?;

    if response.status().is_success() {
        let status_response: VerifyResponse = response.json().await?;
        println!("Verification request sent. ✅");
        println!("Verification in progress... ⏳");
        // Span new thread for polling the server for status
        // Create a channel for communication between threads
        let (sender, receiver) = unbounded();

        let handle = thread::spawn(move || loading_animation(receiver));
        // Poll the server for status
        loop {
            let status = check_job_status(&client, &status_response.request_id).await?;
            match status.status {
                JobStatus::InProgress => {
                    thread::sleep(Duration::from_secs(10));
                }
                JobStatus::Completed => {
                    let _ = sender.send(true);
                    handle.join().unwrap();
                    let status_response = status.respose.unwrap();

                    if status_response.executable_hash == status_response.on_chain_hash {
                        print_verification_status(
                            program_id.to_string().as_str(),
                            true,
                            &status_response,
                        );
                    } else {
                        print_verification_status(
                            program_id.to_string().as_str(),
                            false,
                            &status_response,
                        );
                    }
                    break;
                }
                JobStatus::Failed => {
                    let _ = sender.send(false);

                    handle.join().unwrap();
                    let status_response: JobVerificationResponse = status.respose.unwrap();
                    println!("Program {} has not been verified. ❌", program_id);
                    eprintln!("Error message: {}", status_response.message.as_str());
                    break;
                }
                JobStatus::Unknown => {
                    let _ = sender.send(false);
                    handle.join().unwrap();
                    println!("Program {} has not been verified. ❌", program_id);
                    break;
                }
            }
        }

        Ok(())
    } else if response.status() == 409 {
        let response = response.json::<ErrorResponse>().await?;
        eprintln!("Error: {}", response.error.as_str());
        Ok(())
    } else {
        eprintln!("Encountered an error while attempting to send the job to remote");
        Err(anyhow!("{:?}", response.text().await?))?
    }
}

async fn check_job_status(client: &Client, request_id: &str) -> anyhow::Result<JobResponse> {
    // Get /job/:id
    let response = client
        .get(&format!("{}/job/{}", REMOTE_SERVER_URL, request_id))
        .send()
        .await
        .unwrap();

    if response.status().is_success() {
        // Parse the response
        let response: JobVerificationResponse = response.json().await?;
        match response.status {
            JobStatus::InProgress => {
                thread::sleep(Duration::from_secs(5));
                Ok(JobResponse {
                    status: JobStatus::InProgress,
                    respose: None,
                })
            }
            JobStatus::Completed => Ok(JobResponse {
                status: JobStatus::Completed,
                respose: Some(response),
            }),
            JobStatus::Failed => Ok(JobResponse {
                status: JobStatus::Failed,
                respose: Some(response),
            }),
            JobStatus::Unknown => Ok(JobResponse {
                status: JobStatus::Unknown,
                respose: Some(response),
            }),
        }
    } else {
        Err(anyhow!(
            "Encountered an error while attempting to check job status : {:?}",
            response.text().await?
        ))?
    }
}
