mod hls;
mod util;
mod vimeo;

use anyhow::{bail, Result};
use clap::Parser;
use futures::prelude::*;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Url;
use std::path::PathBuf;
use tokio::io;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let args: Args = Args::parse();
    let cx = Context::new()?;

    let protocol = match args.protocol {
        Protocol::Auto => {
            let url: Url = args.url.parse()?;
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
        Protocol::Vimeo => vimeo::main(args, cx).boxed(),
        Protocol::Hls => hls::main(args, cx).boxed(),
    };

    tokio::select! {
        r = fut => r?,
        r = signal() => r?,
    }

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
    #[arg(long, value_name = "num", default_value_t = 4)]
    parallel_max: usize,
}

#[derive(Debug, Default, Clone, Copy, clap::ValueEnum)]
enum Protocol {
    #[default]
    Auto,
    Vimeo,
    Hls,
}

struct Context {
    progress: ProgressBar,
}

impl Context {
    fn new() -> Result<Self> {
        Ok(Self {
            progress: ProgressBar::new(0).with_style(ProgressStyle::with_template(
                "[{elapsed_precise}] [{wide_bar}] ({eta})",
            )?),
        })
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        self.progress.finish_and_clear()
    }
}
