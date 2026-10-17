use openaction::OpenActionResult;

mod art_resolve;
mod audio;
mod gfx;
mod kick;
mod media_session;
mod memory;
mod reconcile;
mod mixer;
mod mpris;
mod plugin;
mod scroll;
mod utils;
mod window_icons;

#[tokio::main]
async fn main() -> OpenActionResult<()> {
    simplelog::TermLogger::init(
        simplelog::LevelFilter::Debug,
        simplelog::Config::default(),
        simplelog::TerminalMode::Stdout,
        simplelog::ColorChoice::Never,
    )
    .unwrap();

    println!("Starting Volume Controller plugin...");

    plugin::init().await
}
