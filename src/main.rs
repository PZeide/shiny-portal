use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::FmtSubscriber;
use zbus::{Connection, conn::Builder};

use crate::{
    cli::Cli,
    portals::{PORTAL_DBUS_NAME, PORTAL_DBUS_PATH, screen_cast::ScreenCastPortal},
};

mod cli;
mod common;
mod portals;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let cli = Cli::parse();
    let subscriber = FmtSubscriber::builder()
        .with_max_level(if cli.verbose {
            tracing::Level::DEBUG
        } else {
            tracing::Level::INFO
        })
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("failed to init logger");

    let portal_connection = match start_portal().await {
        Ok(conn) => conn,
        Err(err) => {
            error!("failed to start portal: {err}");
            return;
        }
    };

    info!("portal service is running");

    tokio::signal::ctrl_c()
        .await
        .expect("failed to wait for signal");

    info!("portal service is shutting down");
    portal_connection.graceful_shutdown().await;
}

async fn start_portal() -> anyhow::Result<Connection> {
    let connection = Builder::session()?
        .name(PORTAL_DBUS_NAME)?
        .serve_at(PORTAL_DBUS_PATH, ScreenCastPortal::default())?
        .build()
        .await?;

    Ok(connection)
}
