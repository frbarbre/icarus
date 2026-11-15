use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use icarus_server::paths::ServerPaths;
use icarus_server::server::Server;

#[derive(Parser)]
#[command(
    name = "icarus-server",
    version,
    about = "The Icarus server: owns agents, terminals and automations"
)]
struct Cli {
    /// State directory (defaults to $ICARUS_HOME or ~/.icarus).
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server in the foreground on the local socket.
    Serve,
    /// Bridge stdio to this machine's server, starting it if needed. Run over SSH by remote clients.
    Proxy,
}

async fn start_detached(paths: &ServerPaths) -> Result<()> {
    use std::os::unix::process::CommandExt;
    std::fs::create_dir_all(&paths.home)?;
    let log = std::fs::File::create(paths.log())?;
    let _ = std::fs::remove_file(paths.token());
    std::process::Command::new(std::env::current_exe()?)
        .args(["--home", &paths.home.to_string_lossy(), "serve"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !paths.token().exists() {
        if std::time::Instant::now() > deadline {
            anyhow::bail!("server did not start; see {}", paths.log().display());
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    Ok(())
}

async fn proxy(paths: ServerPaths) -> Result<()> {
    use icarus_protocol::{ClientMessage, decode_frames, encode_frame};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let stream = match tokio::net::UnixStream::connect(paths.socket()).await {
        Ok(stream) => stream,
        Err(_) => {
            start_detached(&paths).await?;
            tokio::net::UnixStream::connect(paths.socket()).await?
        }
    };
    let token = std::fs::read_to_string(paths.token())?.trim().to_string();
    let (mut socket_reader, mut socket_writer) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    // The remote client can't read this machine's token; SSH already authenticated it.
    let mut buffer = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = stdin.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..n]);
        let mut messages = decode_frames::<ClientMessage>(&mut buffer)?;
        if messages.is_empty() {
            continue;
        }
        if let ClientMessage::Hello {
            token: presented, ..
        } = &mut messages[0]
        {
            *presented = token.clone();
        }
        for message in messages {
            socket_writer.write_all(&encode_frame(&message)?).await?;
        }
        socket_writer.write_all(&buffer).await?;
        break;
    }
    let upstream = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut stdin, &mut socket_writer).await;
    });
    let _ = tokio::io::copy(&mut socket_reader, &mut stdout).await;
    stdout.flush().await?;
    upstream.abort();
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ICARUS_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let paths = ServerPaths::resolve(cli.home.as_deref())?;
    match cli.command {
        Command::Serve => {
            let (server, listener) = Server::bind(paths.clone()).await?;
            tracing::info!("listening on {}", paths.socket().display());
            server.run(listener).await
        }
        Command::Proxy => proxy(paths).await,
    }
}
