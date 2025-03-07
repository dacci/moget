mod hls;
mod util;
mod vimeo;

use anyhow::{Context as _, Result, bail};
use clap::Parser;
use clap::ValueHint;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Url;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;
use tokio::{io, time};

fn main() -> Result<()> {
    use tracing_subscriber::prelude::*;

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
                .from_env()
                .unwrap(),
        )
        .init();

    let args = Args::parse();

    if let Some(shell) = args.generate_completion {
        use clap::CommandFactory;
        clap_complete::generate(
            shell,
            &mut Args::command(),
            env!("CARGO_BIN_NAME"),
            &mut std::io::stdout(),
        );
        return Ok(());
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::select! {
                r = signal() => r.context("Failed to receive signal"),
                r = async_main(args) => r,
            }
        })
}

async fn async_main(args: Args) -> Result<()> {
    let cx = Context::new();

    let protocol = match args.protocol {
        Protocol::Auto => {
            let url: Url =
                args.url.as_ref().unwrap().parse().with_context(|| {
                    format!("failed to parse URL `{}`", args.url.as_ref().unwrap())
                })?;
            if url.path().ends_with(".json") {
                Protocol::Vimeo
            } else if url.path().ends_with(".m3u8") {
                Protocol::Hls
            } else {
                Protocol::Auto
            }
        }
        protocol => protocol,
    };

    let (files, output) = match protocol {
        Protocol::Auto => bail!("protocol could not be detected"),
        Protocol::Vimeo => vimeo::main(&args, &cx).await?,
        Protocol::Hls => hls::main(&args, &cx).await?,
    };

    let mut command = Command::new("ffmpeg");

    for file in &files {
        command.arg("-i").arg(file);
    }

    command.arg("-codec").arg("copy");

    if let Some(seek) = &args.seek {
        command.arg("-ss").arg(seek);
    }

    if args.fast_start {
        command.arg("-movflags").arg("faststart");
    }

    command.arg(output.as_ref());

    command.spawn()?.wait().await?;

    Ok(())
}

#[cfg(unix)]
async fn signal() -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;

    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
    }

    Ok(())
}

#[cfg(windows)]
async fn signal() -> io::Result<()> {
    use tokio::signal::windows::{ctrl_break, ctrl_c};

    let mut ctrl_c = ctrl_c()?;
    let mut ctrl_break = ctrl_break()?;

    tokio::select! {
        _ = ctrl_c.recv() => {}
        _ = ctrl_break.recv() => {}
    }

    Ok(())
}

#[cfg(not(any(unix, windows)))]
async fn signal() -> io::Result<()> {
    tokio::signal::ctrl_c().await
}

#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Args {
    /// URL of the movie file to download.
    #[arg(required_unless_present = "generate_completion", value_hint = ValueHint::Url)]
    url: Option<String>,

    /// Protocol to use to communicate with the server.
    #[arg(long, default_value = "auto")]
    protocol: Protocol,

    /// Write output to FILE.
    #[arg(short, long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    output: Option<PathBuf>,

    /// Extra header to include in the request.
    #[arg(short = 'H', long = "header", value_name = "X-Name: value")]
    headers: Vec<String>,

    /// For compatibility with cURL, ignored.
    #[arg(long)]
    compressed: bool,

    /// Maximum time in seconds that you allow connection to take.
    #[arg(long, default_value_t = 10.0, value_name = "fractional seconds")]
    connect_timeout: f64,

    /// Maximum time in seconds that you allow single download to take.
    #[arg(short, long, default_value_t = 60.0, value_name = "fractional seconds")]
    max_time: f64,

    /// Set the maximum number of allowed retries attempts.
    #[arg(long, default_value_t = 10, value_name = "num")]
    retry: u32,

    /// Maximum amount of transfers to do simultaneously for each stream.
    #[arg(long, value_name = "num", default_value_t = 1)]
    parallel_max: usize,

    /// Use the specified proxy.
    #[arg(short = 'x', long, value_name = "[protocol://]host[:port]")]
    proxy: Option<String>,

    /// Specify the user name and password to use for proxy authentication.
    #[arg(short = 'U', long, value_name = "user:password")]
    proxy_user: Option<String>,

    /// Discard input until the timestamps reach position.
    #[arg(short, long, value_name = "position")]
    seek: Option<String>,

    /// Run a second pass moving the index (moov atom) to the beginning of the file.
    #[arg(short, long)]
    fast_start: bool,

    /// Choose the lowest quality stream when multiple streams are found.
    #[arg(long)]
    worst: bool,

    /// Skip LEN bytes from the beginning of each segment.
    #[arg(long, value_name = "LEN")]
    skip_bytes: Option<u64>,

    /// Generate shell completions.
    #[arg(long, value_name = "SHELL", exclusive = true)]
    generate_completion: Option<clap_complete::aot::Shell>,
}

#[derive(Debug, Default, Clone, Copy, clap::ValueEnum)]
enum Protocol {
    #[default]
    Auto,
    Vimeo,
    Hls,
}

struct Context {
    progress: Arc<ProgressBar>,
}

impl Context {
    fn new() -> Self {
        let progress = ProgressBar::new(0).with_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] [{wide_bar}] {human_pos:>5}/{human_len:>5} ({eta:>5})",
            )
            .unwrap()
            .progress_chars("=> "),
        );

        Self {
            progress: Arc::new(progress),
        }
    }

    fn start_progress(&self, len: u64) {
        if 0 < len {
            self.progress.set_length(len);
        }
        self.progress.reset();

        let progress = Arc::clone(&self.progress);
        tokio::spawn(async move {
            let mut interval = time::interval(time::Duration::from_millis(500));

            loop {
                interval.tick().await;

                if progress.is_finished() {
                    break;
                }
                progress.tick();
            }
        });
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        self.progress.finish_and_clear()
    }
}
