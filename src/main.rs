use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt};
use zbus::{Connection, conn::Builder};

use crate::{
    cli::Cli,
    config::Config,
    portals::{
        PORTAL_DBUS_NAME, PORTAL_DBUS_PATH, screen_cast::ScreenCastPortal,
        screenshot::ScreenshotPortal,
    },
};

mod cli;
mod common;
mod config;
mod portals;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let cli = Cli::parse();
    let default_filter = if cli.verbose { "debug" } else { "info" };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    fmt().with_env_filter(filter).init();

    let (config, _) = match Config::load_or_create() {
        Ok(config) => config,
        Err(err) => {
            error!("failed to load configuration: {err}");
            return;
        }
    };

    let portal_connection = match start_portal(config).await {
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

async fn start_portal(config: Config) -> anyhow::Result<Connection> {
    Ok(Builder::session()?
        .name(PORTAL_DBUS_NAME)?
        .serve_at(PORTAL_DBUS_PATH, ScreenCastPortal::new(config))?
        .serve_at(PORTAL_DBUS_PATH, ScreenshotPortal::default())?
        .build()
        .await?)
}
