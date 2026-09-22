//! The command line client.
//!
//! Every word of it goes through the same D-Bus interface a panel applet would
//! use, which is the point: if something is awkward here it will be awkward
//! there, and a method nothing calls is a method nobody has tried.
//!
//! Calling any command starts the service if it is not running — D-Bus does
//! that — so `bignetscreenctl cast …` from a keyboard shortcut works on a
//! machine where nothing of ours is running yet.

use std::process::ExitCode;

use futures::StreamExt;

use nd_service::dbus::ServiceProxy;

const USAGE: &str = "\
bignetscreenctl — share a screen from the command line

  list                     receivers found, one per line, tab separated
  status                   what the service is doing
  watch                    follow the status until interrupted
  rescan                   look for receivers again
  cast <id> [source]       start sharing; source is screen (default),
                           window or virtual
  stop                     end whatever is running
  send <id> <file>...      send files to a receiver
  media <command> [value]  toggle-pause, next, seek <seconds>

The service starts on demand and exits a few minutes after the last
session ends, so the screen keeps being shared with no window open.";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: &[String]) -> Result<(), String> {
    let Some(command) = args.first().map(String::as_str) else {
        println!("{USAGE}");
        return Ok(());
    };
    if matches!(command, "-h" | "--help" | "help") {
        println!("{USAGE}");
        return Ok(());
    }

    let connection = zbus::Connection::session()
        .await
        .map_err(|err| format!("no session bus: {err}"))?;
    let service = ServiceProxy::new(&connection)
        .await
        .map_err(|err| format!("the session service is unreachable: {err}"))?;

    match command {
        "list" => {
            for receiver in service.receivers().await.map_err(bus)? {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    receiver.id,
                    receiver.display_name,
                    receiver.kind,
                    receiver.state,
                    receiver.address
                );
            }
        }
        "status" => println!("{}", describe(&service).await?),
        "watch" => {
            println!("{}", describe(&service).await?);
            let mut changes = service.receive_status_changed().await;
            while changes.next().await.is_some() {
                println!("{}", describe(&service).await?);
            }
        }
        "rescan" => service.rescan().await.map_err(bus)?,
        "cast" => {
            let id = args.get(1).ok_or("cast needs the id of a receiver")?;
            let source = args.get(2).map(String::as_str).unwrap_or("monitor");
            service.cast(id, source, &[]).await.map_err(bus)?;
        }
        "stop" => service.stop().await.map_err(bus)?,
        "send" => {
            let id = args.get(1).ok_or("send needs the id of a receiver")?;
            let paths: Vec<String> = args
                .iter()
                .skip(2)
                // Absolute, because the service has its own working directory
                // and a relative path would resolve somewhere else entirely.
                .map(|path| {
                    std::fs::canonicalize(path)
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|_| path.clone())
                })
                .collect();
            if paths.is_empty() {
                return Err("send needs at least one file".into());
            }
            service.send_media(id, &paths).await.map_err(bus)?;
        }
        "media" => {
            let what = args.get(1).ok_or("media needs a command")?;
            let value: f64 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(0.0);
            service.control_media(what, value, "").await.map_err(bus)?;
        }
        other => return Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
    Ok(())
}

/// The status in one line.
///
/// The service reports a shape, never a sentence — it has no locale worth
/// trusting and its clients may each have a different one. This is this
/// client's sentence.
async fn describe(service: &ServiceProxy<'_>) -> Result<String, String> {
    let status = service.status().await.map_err(bus)?;
    let session = service.session().await.map_err(bus)?;
    let line = match status.kind.as_str() {
        "searching" => "searching for receivers".to_string(),
        "found" => format!("{} receiver(s) found", status.count),
        "empty" => "no receivers found".to_string(),
        "discovery-off" => "automatic discovery is off".to_string(),
        "connecting" => format!("connecting to {}", status.display_name),
        "stopping" => "stopping".to_string(),
        "sending" => "sending files".to_string(),
        "ndi-runtime-missing" => "the NDI runtime is not installed".to_string(),
        "error" => format!("error: {}", status.detail),
        "streaming" => {
            let mut line = format!(
                "sharing the {} with {}",
                session.source, status.display_name
            );
            if session.width > 0 {
                line.push_str(&format!(
                    " at {}x{} {} Hz",
                    session.width, session.height, session.fps
                ));
            }
            if session.round_trip_ms > 0 {
                line.push_str(&format!(" ({} ms away)", session.round_trip_ms));
            }
            if !session.url.is_empty() {
                line.push_str(&format!(" — open {} with PIN {}", session.url, session.pin));
            }
            line
        }
        other => other.to_string(),
    };
    Ok(line)
}

fn bus(err: zbus::Error) -> String {
    format!("the session service refused: {err}")
}
