pub mod app;
pub mod ui;

use anyhow::Result;

pub async fn run_tui() -> Result<()> {
    app::run().await
}
