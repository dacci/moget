use crate::util::{tempfile_in, Downloader, SplitBy};
use anyhow::{anyhow, bail, Context, Result};
use futures::prelude::*;
use m3u8_rs::{AlternativeMedia, KeyMethod, MasterPlaylist, Playlist};
use reqwest::Url;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempPath;
use tokio::fs::File;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

type Decryptor = cbc::Decryptor<aes::Aes128>;

pub(super) async fn main(args: super::Args, cx: super::Context) -> Result<()> {
    let client = Downloader::new(&args).context("failed to create HTTP client")?;
    let client = Arc::new(client);

    let url: Url = args
        .url
        .parse()
        .with_context(|| format!("failed to parse URL `{}`", args.url))?;
    let media = resolve_playlist(&client, &url).await?;
    let len = media.iter().fold(0, |a, x| a + x.len());

    let vec = media
        .into_iter()
        .map(|urls| download_merge(&client, urls, &cx.progress))
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

async fn resolve_playlist(
    client: &Downloader,
    url: &Url,
) -> Result<Vec<Vec<(Url, Option<Decryptor>)>>> {
    let playlist = get_playlist(client, url.clone()).await?;

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
                .and_then(|u| get_playlist(client, u.clone()).map_ok(|p| (u, p)))
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
    for (url, list) in vec {
        let mut urls = vec![];
        let mut dec = None;

        for seg in list.segments {
            if let Some(key) = &seg.key {
                dec = build_decryptor(key, &url, client).await?;
            }

            if let Some(map) = &seg.map {
                let url = url
                    .join(&map.uri)
                    .with_context(|| format!("failed to build URL from `{}`", map.uri))?;
                urls.push((url, dec.clone()));
            }

            let url = url
                .join(&seg.uri)
                .with_context(|| format!("failed to build URL from `{}`", seg.uri))?;
            urls.push((url, dec.clone()));
        }

        sources.push(urls);
    }

    Ok(sources)
}

async fn get_playlist(client: &Downloader, url: Url) -> Result<Playlist> {
    let res = client
        .get_bytes(url.clone())
        .await
        .with_context(|| format!("failed to get playlist from {url}"))?;

    match m3u8_rs::parse_playlist(&res) {
        Ok((_, playlist)) => Ok(playlist),
        Err(_) => Err(anyhow!("failed to parse playlist content")),
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

async fn build_decryptor(
    key_info: &m3u8_rs::Key,
    url: &Url,
    client: &Downloader,
) -> Result<Option<Decryptor>> {
    use cipher::KeyIvInit;

    match &key_info.method {
        KeyMethod::None => return Ok(None),
        KeyMethod::AES128 => {}
        method => bail!("unsupported encryption method `{method}`"),
    }

    let iv = key_info
        .iv
        .as_deref()
        .ok_or_else(|| anyhow!("IV is required but not provided"))
        .and_then(parse_iv)?;

    let key_url = key_info
        .uri
        .as_deref()
        .ok_or_else(|| anyhow!("key URI is required but not provided"))
        .and_then(|s| {
            url.join(s)
                .with_context(|| anyhow!("failed to build key URL from `{s}`"))
        })?;

    let key = client
        .get_bytes(key_url.clone())
        .await
        .with_context(|| anyhow!("failed to get key from {key_url}"))?;

    Decryptor::new_from_slices(&key, &iv)
        .map_err(anyhow::Error::from)
        .map(Some)
}

fn parse_iv(src: &str) -> Result<Vec<u8>> {
    if src.len() != 34 {
        bail!("illegal IV length");
    }

    let mut it = SplitBy::new(src, 2);

    match it.next().unwrap() {
        "0X" | "0x" => {}
        s => bail!("IV must start with `0x`, got `{s}`"),
    }

    it.map(|s| u8::from_str_radix(s, 16).map_err(anyhow::Error::from))
        .collect()
}

async fn download_merge(
    client: &Arc<Downloader>,
    urls: Vec<(Url, Option<Decryptor>)>,
    progress: &indicatif::ProgressBar,
) -> Result<TempPath> {
    let (file, path) = tempfile_in(".")
        .await
        .context("failed to create temporary file for merge")?;

    stream::iter(urls)
        .then(|(url, dec)| {
            let client = Arc::clone(client);
            #[allow(clippy::async_yields_async)]
            async move {
                client
                    .download(url)
                    .and_then(|path| async move { Ok((path, dec)) })
            }
        })
        .buffered(client.parallel_max)
        .try_fold(file, |mut dest, (src, dec)| async move {
            decrypt_merge(src, dec, &mut dest).await?;
            progress.inc(1);
            Ok(dest)
        })
        .await?;

    Ok(path)
}

async fn decrypt_merge(
    src: impl AsRef<Path>,
    dec: Option<Decryptor>,
    dest: &mut (impl io::AsyncWrite + Unpin + ?Sized),
) -> Result<()> {
    let src = src.as_ref();
    let mut src_file = File::open(src)
        .await
        .with_context(|| format!("failed to open `{}`", src.display()))?;

    if let Some(dec) = dec {
        decrypt_copy(dec, &mut src_file, dest)
            .await
            .with_context(|| format!("failed to merge `{}`", src.display()))?;
    } else {
        io::copy(&mut src_file, dest)
            .await
            .with_context(|| format!("failed to merge `{}`", src.display()))?;
    }

    Ok(())
}

async fn decrypt_copy(
    mut dec: Decryptor,
    src: &mut (impl io::AsyncRead + Unpin + ?Sized),
    dest: &mut (impl io::AsyncWrite + Unpin + ?Sized),
) -> io::Result<()> {
    use cipher::BlockDecryptMut;

    let mut prev = None;
    loop {
        let mut block = aes::Block::from([0; 16]);
        match src.read(&mut block).await? {
            16 => {}
            0 => break,
            _ => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
        };

        if let Some(mut block) = prev.replace(block) {
            dec.decrypt_block_mut(&mut block);
            dest.write_all(&block).await?;
        }
    }

    match prev {
        None => Ok(()),
        Some(mut block) => {
            let block = dec
                .decrypt_padded_mut::<block_padding::Pkcs7>(&mut block)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            dest.write_all(block).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_iv() {
        assert!(parse_iv("").is_err());
        assert!(parse_iv("01234567890123456789012345678901234").is_err());
        assert_eq!(
            parse_iv("0x0123456789ABCDEF0123456789ABCDEF").unwrap(),
            &[
                0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, //
                0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF,
            ]
        )
    }
}
