pub mod app;
pub mod keymap;
pub mod menu;
pub mod popup;
pub mod ui;

use anyhow::Result;

pub async fn run_tui() -> Result<()> {
    app::run().await
}
