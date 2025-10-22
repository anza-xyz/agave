use {
    anyhow::Result,
    log::{debug, info},
};

pub fn run() -> Result<()> {
    debug!("DEBUG MODE");
    info!("Hello!");
    Ok(())
}
