mod client;
mod solana;
mod models;

pub use client::send_job_to_remote;
pub use solana::get_last_deployed_slot;