use std::time::Duration;

use mail_server::alarm::send_alarm;
use mail_server::postgres::initialize::{
    boot_maintenance, initialize_domains, initialize_postgres,
};
use mail_server::postgres::pool;
use mail_server::{http, push, smtp};
use tokio_util::sync::CancellationToken;

/// Ceiling on the startup-failure alarm, so a wedged webhook cannot hold the exit.
const CRASH_ALARM_TIMEOUT: Duration = Duration::from_secs(5);

async fn start() -> anyhow::Result<()> {
    // Settings only: a misconfiguration stops the start here, before anything
    // binds, and the provider is first contacted by the first sign-in.
    mail_server::sso::initialize_sso()?;
    initialize_postgres().await?;
    initialize_domains().await?;
    push::init_push()?;
    let http_server = http::initialize_http().await?;
    // Sweeps `attachments/.incoming`, then opens SMTP_PORT, and with a
    // certificate SMTPS_PORT and SMTP_SUBMISSION_PORT.
    let smtp_servers = smtp::initialize_mail_smtp().await?;
    let imap_servers = mail_server::imap::initialize_imap().await?;
    push::clean_subscriptions();

    // Index builds and the search-vector reindex scale with the size of
    // `mails`, so they run after the listeners are bound rather than in
    // front of them.
    let maintenance_cancel = CancellationToken::new();
    let maintenance = tokio::spawn(boot_maintenance(maintenance_cancel.clone()));

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let signal = tokio::select! {
        _ = sigterm.recv() => "SIGTERM",
        _ = tokio::signal::ctrl_c() => "SIGINT",
    };
    println!("{signal} received — shutting down gracefully");

    // First: cancelling an in-flight index build is a round trip on another
    // connection, so it overlaps the server closes below.
    maintenance_cancel.cancel();

    http_server.stop().await;
    println!("HTTP server closed");

    // Notify IDLE clients and stop the heartbeat before closing sockets.
    mail_server::imap::idle_manager::shutdown();
    println!("IDLE sessions cleaned up");

    // Each protocol ends its own connections: IMAP sends BYE to every session
    // and tears it down, SMTP gives in-flight DATA SMTP_CLOSE_TIMEOUT_MS
    // before answering the rest 421. They run at once, so this phase costs
    // the longer bound rather than the sum.
    let close_imap = async {
        if mail_server::imap::close_imap_servers(imap_servers).await {
            println!("IMAP servers closed");
        } else {
            println!("IMAP servers did not drain in time; continuing shutdown");
        }
    };
    let close_smtp = async {
        smtp::close_smtp_servers(&smtp_servers).await;
        println!("SMTP servers closed");
    };
    tokio::join!(close_imap, close_smtp);

    let _ = maintenance.await;
    println!("Boot maintenance stopped");

    pool().end();
    println!("Database pool closed");
    Ok(())
}

fn main() {
    mail_server::alarm::install_panic_alarm();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // Deeply nested client JSON is parsed and dropped recursively.
        .thread_stack_size(16 * 1024 * 1024)
        .build()
        .expect("the runtime starts");
    let outcome = runtime.block_on(async {
        match start().await {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("Fatal error during startup: {error}");
                let detail = format!("**Message:** {error}\n```\n{error:?}\n```");
                let _ = tokio::time::timeout(CRASH_ALARM_TIMEOUT, send_alarm("Startup Failed", &detail, None)).await;
                1
            }
        }
    });
    // Background tasks hold timers that would keep the runtime alive.
    runtime.shutdown_background();
    std::process::exit(outcome);
}
