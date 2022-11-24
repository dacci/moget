mod hls;
mod util;
mod vimeo;

use anyhow::{bail, Context as _, Result};
use clap::Parser;
use futures::prelude::*;
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

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async_main(args))
}

async fn async_main(args: Args) -> Result<()> {
    let cx = Context::new();

    let protocol = match args.protocol {
        Protocol::Auto => {
            let url: Url = args
                .url
                .parse()
                .with_context(|| format!("failed to parse URL `{}`", args.url))?;
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

    let fut = match protocol {
        Protocol::Auto => bail!("protocol could not be detected"),
        Protocol::Vimeo => vimeo::main(&args, &cx).boxed(),
        Protocol::Hls => hls::main(&args, &cx).boxed(),
    };

    let (files, output) = tokio::select! {
        r = fut => r?,
        r = signal() => match r {
            Ok(_) => bail!("Ctrl+C"),
            Err(e) => return Err(e.into()),
        },
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
    use tokio::signal::unix::{signal, SignalKind};

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
    url: String,

    /// Protocol to use to communicate with the server.
    #[arg(long, default_value = "auto")]
    protocol: Protocol,

    /// Write output to FILE.
    #[arg(short, long, value_name = "FILE")]
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
        self.progress.set_length(len);
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
