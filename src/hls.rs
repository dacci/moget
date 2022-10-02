use crate::util::Downloader;
use anyhow::{anyhow, Result};
use futures::prelude::*;
use m3u8_rs::{AlternativeMedia, MasterPlaylist, Playlist};
use reqwest::Url;
use std::path::Path;
use std::sync::Arc;
use tokio::process::Command;

pub(super) async fn main(args: super::Args, cx: super::Context) -> Result<()> {
    let client = Arc::new(Downloader::new(&args)?);

    let url: Url = args.url.parse()?;
    let media = resolve_playlist(&client, &url).await?;
    let len = media.iter().fold(0, |a, x| a + x.len());

    let vec = media
        .into_iter()
        .map(|urls| client.download_merge(urls, &cx.progress))
        .collect::<Vec<_>>();

    cx.start_progress(len as _);
    let files = future::try_join_all(vec).await?;
    cx.progress.finish();

    let mut command = Command::new("ffmpeg");

    for path in &files {
        command.arg("-i").arg(path);
    }

    command.arg("-codec").arg("copy");

    if let Some(output) = &args.output {
        command.arg(output);
    } else {
        let output = url.path_segments().unwrap().last().unwrap();
        let output = Path::new(output).with_extension("mp4");
        command.arg(output);
    };

    command.spawn()?.wait().await?;

    Ok(())
}

async fn resolve_playlist(client: &Downloader, url: &Url) -> Result<Vec<Vec<Url>>> {
    let playlist = get_playlist(client, url).await?;

    let vec = match playlist {
        Playlist::MasterPlaylist(playlist) => {
            let best = playlist
                .variants
                .iter()
                .filter(|v| !v.is_i_frame)
                .max_by_key(|v| v.bandwidth)
                .ok_or_else(|| anyhow!("no suitable variant found in the master playlist"))?;

            let alt_video = best
                .video
                .as_ref()
                .and_then(|n| find_alternative(&playlist, n))
                .and_then(|a| a.uri.as_ref());
            let alt_audio = best
                .audio
                .as_ref()
                .and_then(|n| find_alternative(&playlist, n))
                .and_then(|a| a.uri.as_ref());

            let s = if alt_video.is_some() && alt_audio.is_some() {
                alt_video
                    .into_iter()
                    .chain(alt_audio.into_iter())
                    .collect::<Vec<_>>()
                    .into_iter()
            } else {
                alt_video
                    .into_iter()
                    .chain(alt_audio.into_iter())
                    .chain(Some(&best.uri))
                    .collect::<Vec<_>>()
                    .into_iter()
            }
            .map(|s| url.join(s).map_err(anyhow::Error::new));
            stream::iter(s)
                .and_then(|u| async { get_playlist(client, &u).await.map(|p| (u, p)) })
                .and_then(|(u, p)| async {
                    match p {
                        Playlist::MasterPlaylist(_) => Err(anyhow!("not a media playlist")),
                        Playlist::MediaPlaylist(p) => Ok((u, p)),
                    }
                })
                .try_collect::<Vec<_>>()
                .await?
        }
        Playlist::MediaPlaylist(playlist) => vec![(url.clone(), playlist)],
    };

    let mut sources = vec![];
    for (u, list) in vec {
        let mut urls = vec![];

        for seg in list.segments {
            if let Some(map) = &seg.map {
                urls.push(u.join(&map.uri)?);
            }

            urls.push(u.join(&seg.uri)?);
        }

        sources.push(urls);
    }

    Ok(sources)
}

async fn get_playlist(client: &Downloader, url: &Url) -> Result<Playlist> {
    let res = client
        .get(url.clone())
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    match m3u8_rs::parse_playlist(&res) {
        Ok((_, playlist)) => Ok(playlist),
        Err(e) => Err(anyhow!("{e}")),
    }
}

fn find_alternative<'a>(
    playlist: &'a MasterPlaylist,
    name: &'a str,
) -> Option<&'a AlternativeMedia> {
    let mut candidates = playlist.alternatives.iter().filter(|a| a.group_id.eq(name));

    if let Some(alt) = candidates.clone().find(|a| a.default) {
        return Some(alt);
    }

    if let Some(alt) = candidates.clone().find(|a| a.autoselect) {
        return Some(alt);
    }

    candidates.next()
}
