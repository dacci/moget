use crate::util::Downloader;
use anyhow::{bail, Result};
use m3u8_rs::Playlist;
use reqwest::Url;
use std::path::Path;
use std::sync::Arc;
use tokio::process::Command;

pub(super) async fn main(args: super::Args, cx: super::Context) -> Result<()> {
    let client = Arc::new(Downloader::new(&args)?);

    let url: Url = args.url.parse()?;
    let res = client
        .get(url.clone())
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    let playlist = match m3u8_rs::parse_playlist(&res) {
        Ok((_, playlist)) => match playlist {
            Playlist::MasterPlaylist(_) => bail!("not a media playlist"),
            Playlist::MediaPlaylist(playlist) => playlist,
        },
        Err(e) => bail!("{e}"),
    };

    let mut vec = vec![];
    for segment in &playlist.segments {
        if let Some(map) = &segment.map {
            vec.push(url.join(&map.uri)?)
        }

        vec.push(url.join(&segment.uri)?);
    }

    cx.progress.set_length(vec.len() as _);
    let path = client.download_merge(vec, &cx.progress).await?;

    let mut command = Command::new("ffmpeg");
    command
        .arg("-i")
        .arg::<&Path>(path.as_ref())
        .arg("-codec")
        .arg("copy")
        .arg(args.output.unwrap());

    command.spawn()?.wait().await?;

    Ok(())
}
