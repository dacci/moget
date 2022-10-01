use crate::util::Downloader;
use anyhow::Result;
use futures::prelude::*;
use reqwest::Url;
use std::sync::Arc;
use tempfile::TempPath;
use tokio::process::Command;

#[derive(serde::Deserialize)]
struct Clip {
    clip_id: String,
    base_url: String,
    video: Option<Vec<Media>>,
    audio: Option<Vec<Media>>,
}

impl Clip {
    fn best_video(&self) -> Option<&Media> {
        self.video.as_ref().and_then(|s| Media::best_of(s))
    }

    fn best_audio(&self) -> Option<&Media> {
        self.audio.as_ref().and_then(|s| Media::best_of(s))
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

    fn resolve(&self, clip_url: &Url) -> Result<Vec<Url>> {
        let base_url = clip_url.join(&self.base_url)?;
        let mut urls = vec![base_url.join(&self.init_segment)?];

        if let Some(url) = &self.index_segment {
            urls.push(base_url.join(url)?);
        }

        for segment in &self.segments {
            urls.push(base_url.join(&segment.url)?);
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

pub(super) async fn main(args: super::Args) -> Result<()> {
    let client = Arc::new(Downloader::new(&args)?);

    let master_url = build_master_url(&args.url)?;

    let clip: Clip = client
        .get(master_url.clone())
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let clip_url = master_url.join(&clip.base_url)?;

    let mut vec = vec![];
    if let Some(media) = clip.best_video() {
        let urls = media.resolve(&clip_url)?;
        vec.push(client.download_merge(urls));
    }
    if let Some(media) = clip.best_audio() {
        let urls = media.resolve(&clip_url)?;
        vec.push(client.download_merge(urls));
    }

    let files: Vec<TempPath> = future::try_join_all(vec).await?;

    let mut command = Command::new("ffmpeg");

    for file in &files {
        command.arg("-i").arg(file);
    }

    command.arg("-codec").arg("copy");

    if let Some(output) = args.output {
        command.arg(output);
    } else {
        command.arg(format!("{}.mp4", clip.clip_id));
    }

    command.spawn()?.wait().await?;

    Ok(())
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
