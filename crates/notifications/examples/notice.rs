//! Shows one notification and waits up to 30 s for a click.
//!
//! ```sh
//! cargo run -p notifications --example notice -- "Bob Example" "Quarterly report"
//! ```

// A command-line tool: it reports what happened.
#![allow(clippy::print_stdout)]

use std::time::Duration;

use notifications::{AppInfo, Notice, Notifier};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter("debug").init();
    let mut args = std::env::args().skip(1);
    let title = args.next().unwrap_or_else(|| "Bob Example".to_owned());
    let body = args
        .next()
        .unwrap_or_else(|| "Quarterly <numbers> & more".to_owned());
    let notifier = Notifier::new(
        AppInfo {
            name: "Maze Dev".to_owned(),
            id: "dev.mazemail.maze-dev".to_owned(),
            icon_dir: Some(std::env::temp_dir().join("mazemail-notice-example")),
        },
        tokio::runtime::Handle::current(),
    );
    let (clicked, mut rx) = tokio::sync::mpsc::unbounded_channel();
    notifier.show(
        Notice {
            title,
            subtitle: Some("Work".to_owned()),
            body: Some(body),
            group: None,
        },
        move || {
            let _ = clicked.send(());
        },
    );
    match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
        Ok(Some(())) => println!("clicked"),
        _ => println!("no click"),
    }
}
