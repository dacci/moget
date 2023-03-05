use crate::util::{tempfile_in, Downloader};
use anyhow::{Context, Result};
use futures::prelude::*;
use indicatif::ProgressBar;
use reqwest::Url;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::vec::IntoIter;
use tempfile::TempPath;
use tokio::fs::File;
use tokio::io;
use tracing::debug;

#[derive(serde::Deserialize)]
struct Clip {
    clip_id: String,
    base_url: String,
    video: Option<Vec<Media>>,
    audio: Option<Vec<Media>>,
}

impl Clip {
    fn best_video(&self) -> Option<&Media> {
        self.video.as_deref().and_then(Media::best_of)
    }

    fn worst_video(&self) -> Option<&Media> {
        self.video.as_deref().and_then(Media::worst_of)
    }

    fn best_audio(&self) -> Option<&Media> {
        self.audio.as_deref().and_then(Media::best_of)
    }

    fn worst_audio(&self) -> Option<&Media> {
        self.audio.as_deref().and_then(Media::worst_of)
    }

    fn iter(&self, worst: bool) -> IntoIter<&Media> {
        let (video, audio) = if worst {
            (self.worst_video(), self.worst_audio())
        } else {
            (self.best_video(), self.best_audio())
        };

        video
            .into_iter()
            .chain(audio)
            .collect::<Vec<_>>()
            .into_iter()
    }
}

#[derive(serde::Deserialize)]
struct Media {
    // id: String,
    base_url: String,
    // format: String,
    // mime_type: String,
    // codecs: String,
    bitrate: u32,
    // avg_bitrate: u32,
    // duration: f64,
    // framerate: f64,
    // width: u32,
    // height: u32,
    // channels: u32,
    // sample_rate: u32,
    // max_segment_duration: u32,
    init_segment: String,
    // init_segment_range: Option<String>,
    index_segment: Option<String>,
    // index_segment_range: Option<String>,
    segments: Vec<Segment>,
}

impl Media {
    fn best_of(streams: &[Self]) -> Option<&Self> {
        streams.iter().max_by_key(|s| s.bitrate)
    }

    fn worst_of(streams: &[Self]) -> Option<&Self> {
        streams.iter().min_by_key(|s| s.bitrate)
    }

    fn resolve(&self, clip_url: &Url) -> Result<Vec<Url>> {
        let base_url = clip_url
            .join(&self.base_url)
            .with_context(|| format!("failed to build media URL from `{}`", self.base_url))?;
        let mut urls = vec![base_url.join(&self.init_segment).with_context(|| {
            format!(
                "failed to build init segment URL from `{}`",
                self.init_segment
            )
        })?];

        if let Some(url) = &self.index_segment {
            urls.push(
                base_url
                    .join(url)
                    .with_context(|| format!("failed build index segment URL from `{url}`"))?,
            );
        }

        for segment in &self.segments {
            urls.push(
                base_url.join(&segment.url).with_context(|| {
                    format!("failed to build segment URL from `{}`", segment.url)
                })?,
            );
        }

        Ok(urls)
    }
}

#[derive(serde::Deserialize)]
struct Segment {
    // start: f64,
    // end: f64,
    url: String,
    // size: u64,
}

pub(super) async fn main<'a>(
    args: &'a super::Args,
    cx: &'a super::Context,
) -> Result<(Vec<TempPath>, Cow<'a, Path>)> {
    let client = Downloader::new(args).context("failed to create HTTP client")?;
    let client = Arc::new(client);

    let master_url = build_master_url(&args.url)
        .with_context(|| format!("failed to build master URL from `{}`", args.url))?;

    let clip: Clip = client
        .get(master_url.clone())
        .send()
        .and_then(|r| async { r.error_for_status() }.err_into())
        .and_then(|r| r.json().err_into())
        .await
        .with_context(|| format!("failed to get clip info from `{master_url}`"))?;
    let clip_url = master_url
        .join(&clip.base_url)
        .with_context(|| format!("failed to build clip base URL from `{}`", clip.base_url))?;

    let mut len = 0;
    let vec = clip
        .iter(args.worst)
        .map(|media| {
            media.resolve(&clip_url).map(|urls| {
                len += urls.len();
                download_merge(&client, urls, args.parallel_max, &cx.progress)
            })
        })
        .collect::<Result<Vec<_>>>()?;

    cx.start_progress(len as _);
    let files: Vec<TempPath> = future::try_join_all(vec).await?;
    cx.progress.finish();

    let output = if let Some(output) = &args.output {
        output.into()
    } else {
        PathBuf::from(format!("{}.mp4", clip.clip_id)).into()
    };

    Ok((files, output))
}

fn build_master_url(url: &str) -> Result<Url> {
    let mut url: Url = url.parse()?;

    url.query_pairs_mut()
        .clear()
        .append_pair("query_string_ranges", "1")
        .append_pair("base64_init", "0")
        .finish();

    Ok(url)
}

async fn download_merge(
    client: &Arc<Downloader>,
    urls: Vec<Url>,
    parallel_max: usize,
    progress: &ProgressBar,
) -> Result<TempPath> {
    let (file, path) = tempfile_in(".")
        .await
        .context("failed to create temporary file for merge")?;

    stream::iter(urls)
        .map(|url| {
            let this = Arc::clone(client);
            this.download(url)
        })
        .buffered(parallel_max)
        .try_fold(file, |mut dest, src| async move {
            merge(src, &mut dest).await?;
            progress.inc(1);
            Ok(dest)
        })
        .await?;

    Ok(path)
}

async fn merge(
    src: impl AsRef<Path>,
    dest: &mut (impl io::AsyncWrite + Unpin + ?Sized),
) -> Result<()> {
    let src = src.as_ref();
    let mut src_file = File::open(src)
        .await
        .with_context(|| format!("failed to open `{}`", src.display()))?;
    io::copy(&mut src_file, dest)
        .await
        .with_context(|| format!("failed to merge `{}`", src.display()))?;

    debug!(target: "merge", "{}", src.display());

    Ok(())
}
