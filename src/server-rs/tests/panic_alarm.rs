//! A panic pages "Uncaught Exception", the alarm the Bun server sends for an
//! uncaught exception, instead of reaching nothing but stderr.
//!
//! Its own test binary: it installs a process-wide panic hook and points
//! `DISCORD_ALARM_WEBHOOK` at a local listener.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_task_pages_uncaught_exception() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a webhook listener");
    let webhook = format!("http://{}/alarm", listener.local_addr().expect("an address"));
    std::env::set_var("DISCORD_ALARM_WEBHOOK", &webhook);
    mail_server::alarm::install_panic_alarm();

    let task = tokio::spawn(async { panic!("boom in a connection task") });
    assert!(task.await.is_err(), "the task panicked");

    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("the alarm is posted")
        .expect("a connection");
    let mut request = Vec::new();
    let mut chunk = [0u8; 4096];
    while !request.ends_with(b"}]}") {
        let read = tokio::time::timeout(Duration::from_secs(10), socket.read(&mut chunk))
            .await
            .expect("the request arrives")
            .expect("a read");
        assert!(read > 0, "the request ended early: {}", String::from_utf8_lossy(&request));
        request.extend_from_slice(&chunk[..read]);
    }
    socket.write_all(b"HTTP/1.1 204 No Content\r\n\r\n").await.expect("an answer");

    let request = String::from_utf8_lossy(&request);
    assert!(request.starts_with("POST /alarm "), "{request}");
    assert!(request.contains(r#""title":"🚨 Mail Server Error: Uncaught Exception""#), "{request}");
    assert!(request.contains(r#""description":"**Message:** boom in a connection task\n```\ntests/panic_alarm.rs:"#), "{request}");
}
