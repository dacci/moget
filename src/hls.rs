use crate::util::{Downloader, SplitBy, tempfile_in};
use anyhow::{Context as _, Result, anyhow, bail};
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::prelude::*;
use m3u8_rs::{AlternativeMedia, KeyMethod, MasterPlaylist, MediaPlaylist, Playlist};
use reqwest::Url;
use std::borrow::Cow;
use std::future::IntoFuture;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::vec::IntoIter;
use tempfile::TempPath;
use tokio::fs::File;
use tokio::io::{self, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::sync::{Notify, RwLock};
use tokio::time::{Duration, Sleep, sleep};
use tracing::warn;

type Decryptor = cbc::Decryptor<aes::Aes128>;

pub(super) async fn main<'a>(
    args: &'a super::Args,
    cx: &'a super::Context,
) -> Result<(Vec<TempPath>, Cow<'a, Path>)> {
    let client = Downloader::new(args).context("failed to create HTTP client")?;
    let client = Arc::new(client);

    let url: Url = args
        .url
        .as_ref()
        .unwrap()
        .parse()
        .with_context(|| format!("failed to parse URL `{}`", args.url.as_ref().unwrap()))?;
    let media = resolve_playlist(&client, &url, args.worst).await?;

    let vec = media
        .into_iter()
        .map(|stream| {
            stream.download_merge(&client, args.parallel_max, args.skip_bytes, &cx.progress)
        })
        .collect::<Vec<_>>();

    cx.start_progress(0);
    let files = future::try_join_all(vec).await?;
    cx.progress.finish();

    let output = if let Some(output) = &args.output {
        output.into()
    } else {
        let output = url.path_segments().unwrap().next_back().unwrap();
        Path::new(output).with_extension("mp4").into()
    };

    Ok((files, output))
}

async fn resolve_playlist<'a>(
    client: &Arc<Downloader>,
    url: &Url,
    worst: bool,
) -> Result<Vec<SegmentStream<'a>>> {
    let playlist = get_playlist(client, url.clone()).await?;

    match playlist {
        Playlist::MasterPlaylist(playlist) => {
            let candidates = playlist.variants.iter().filter(|v| !v.is_i_frame);
            let stream = if worst {
                candidates.min_by_key(|v| v.bandwidth)
            } else {
                candidates.max_by_key(|v| v.bandwidth)
            }
            .ok_or_else(|| anyhow!("no suitable variant found in the master playlist"))?;

            let alt_video = stream
                .video
                .as_ref()
                .and_then(|n| find_alternative(&playlist, n))
                .and_then(|a| a.uri.as_ref());
            let alt_audio = stream
                .audio
                .as_ref()
                .and_then(|n| find_alternative(&playlist, n))
                .and_then(|a| a.uri.as_ref());

            if alt_video.is_some() && alt_audio.is_some() {
                alt_video
                    .into_iter()
                    .chain(alt_audio.into_iter())
                    .collect::<Vec<_>>()
                    .into_iter()
            } else {
                alt_video
                    .into_iter()
                    .chain(alt_audio.into_iter())
                    .chain(Some(&stream.uri))
                    .collect::<Vec<_>>()
                    .into_iter()
            }
            .map(|s| {
                url.join(s)
                    .map(|u| SegmentStream::new(client, u))
                    .map_err(anyhow::Error::new)
            })
            .collect()
        }
        Playlist::MediaPlaylist(playlist) => {
            let mut stream = SegmentStream::new(client, url.clone());
            stream.feed_playlist(playlist)?;
            Ok(vec![stream])
        }
    }
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

async fn decrypt_merge<W>(
    src: impl AsRef<Path>,
    skip: Option<u64>,
    dec: Option<Decryptor>,
    dest: &mut W,
) -> Result<()>
where
    W: io::AsyncWrite + Unpin + ?Sized,
{
    let src = src.as_ref();
    let mut src_file = File::open(src)
        .await
        .with_context(|| format!("failed to open `{}`", src.display()))?;

    if let Some(dec) = dec {
        decrypt_copy(dec, &mut src_file, skip, dest)
            .await
            .with_context(|| format!("failed to merge `{}`", src.display()))?;
    } else {
        if let Some(len) = skip {
            src_file.seek(SeekFrom::Start(len)).await?;
        }

        io::copy(&mut src_file, dest)
            .await
            .with_context(|| format!("failed to merge `{}`", src.display()))?;
    }

    Ok(())
}

async fn decrypt_copy<R, W>(
    mut dec: Decryptor,
    src: &mut R,
    skip: Option<u64>,
    dest: &mut W,
) -> io::Result<()>
where
    R: io::AsyncRead + Unpin + ?Sized,
    W: io::AsyncWrite + Unpin + ?Sized,
{
    use cipher::BlockDecryptMut;

    let mut src = io::BufReader::new(src);
    let mut skip = skip.unwrap_or_default();
    let mut dest = io::BufWriter::new(dest);

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

            if skip > 0 {
                let len = block.len().min(skip as _);
                if len > 0 {
                    dest.write_all(&block[len..]).await?;
                }
                skip -= len as u64;
            } else {
                dest.write_all(&block).await?;
            }
        }
    }

    if let Some(mut block) = prev {
        let block = dec
            .decrypt_padded_mut::<block_padding::Pkcs7>(&mut block)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        dest.write_all(block).await?;
    }

    dest.flush().await
}

#[derive(Clone)]
struct KeyIv {
    #[allow(clippy::type_complexity)]
    inner: Arc<RwLock<Poll<Result<(Bytes, Bytes)>>>>,
    notify: Arc<Notify>,
}

impl KeyIv {
    fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Poll::Pending)),
            notify: Arc::new(Notify::new()),
        }
    }
}

impl IntoFuture for KeyIv {
    type Output = Result<Decryptor>;
    type IntoFuture = BoxFuture<'static, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        async move {
            loop {
                if let Poll::Ready(res) = self.inner.read().await.deref() {
                    match res {
                        Ok((key, iv)) => {
                            use cipher::KeyIvInit;
                            break Ok(Decryptor::new_from_slices(key, iv)?);
                        }
                        Err(e) => break Err(anyhow!("{e}")),
                    }
                }

                self.notify.notified().await;
            }
        }
        .boxed()
    }
}

struct SegmentStream<'a> {
    client: Arc<Downloader>,
    url: Url,
    sleep: Option<Pin<Box<Sleep>>>,
    request: Option<BoxFuture<'a, Result<MediaPlaylist>>>,
    valid: bool,
    end_list: bool,
    iter: Option<IntoIter<Segment>>,
    seq: Option<u64>,
    len: u64,
    bar: Option<&'a indicatif::ProgressBar>,
}

impl<'a> SegmentStream<'a> {
    fn new(client: &Arc<Downloader>, url: Url) -> Self {
        Self {
            client: Arc::clone(client),
            url,
            sleep: None,
            request: None,
            valid: false,
            end_list: false,
            iter: None,
            seq: None,
            len: 0,
            bar: None,
        }
    }

    fn set_progress_bar(&mut self, bar: &'a indicatif::ProgressBar) {
        bar.inc_length(self.len);
        self.bar = Some(bar);
    }

    fn feed_playlist(&mut self, playlist: MediaPlaylist) -> Result<u64> {
        let end_list = playlist.end_list;
        let vec = self.resolve_segments(playlist)?;
        let len = vec.len() as u64;

        self.len += len;
        if let Some(bar) = self.bar {
            bar.inc_length(len);
        }

        self.valid = true;
        self.end_list = end_list;
        self.iter = Some(vec.into_iter());

        Ok(len)
    }

    fn get_playlist(&self) -> impl Future<Output = Result<MediaPlaylist>> + use<> {
        self.client
            .get(self.url.clone())
            .send()
            .and_then(|r| async { r.error_for_status() }.err_into())
            .and_then(|r| r.bytes().err_into())
            .err_into()
            .and_then(|body| async move { Self::parse_media_playlist(&body) })
    }

    fn parse_media_playlist(bytes: &[u8]) -> Result<MediaPlaylist> {
        match m3u8_rs::parse_media_playlist(bytes) {
            Ok((_, playlist)) => Ok(playlist),
            Err(_) => Err(anyhow!("failed to parse media playlist")),
        }
    }

    fn resolve_segments(&self, playlist: MediaPlaylist) -> Result<Vec<Segment>> {
        let mut vec = Vec::new();
        let mut key_iv = None;

        for (seg, seq) in playlist.segments.into_iter().zip(playlist.media_sequence..) {
            if let Some(pos) = self.seq
                && seq <= pos
            {
                continue;
            }

            if let Some(key_info) = seg.key {
                key_iv = self.resolve_key(key_info)?;
            }

            if let Some(map) = seg.map {
                let url = self.url.join(&map.uri)?;
                vec.push(Segment::new(seq, url, key_iv.clone()));
            }

            let url = self.url.join(&seg.uri)?;
            vec.push(Segment::new(seq, url, key_iv.clone()));
        }

        Ok(vec)
    }

    fn resolve_key(&self, key_info: m3u8_rs::Key) -> Result<Option<KeyIv>> {
        match key_info.method {
            KeyMethod::AES128 => {
                let url = key_info
                    .uri
                    .ok_or_else(|| anyhow!("key URI is required but not provided"))
                    .and_then(|u| {
                        self.url
                            .join(&u)
                            .with_context(|| format!("invalid key uri `{u}`"))
                    })?;
                let iv = key_info
                    .iv
                    .ok_or_else(|| anyhow!("IV is required but not provided"))
                    .and_then(|i| parse_iv(&i))
                    .map(Bytes::from)?;
                let key_iv = KeyIv::new();

                let client = Arc::clone(&self.client);
                let key_iv_inner = key_iv.clone();
                tokio::spawn(async move {
                    let res = client
                        .get_bytes(url)
                        .map_ok(|key| (key, iv))
                        .err_into()
                        .await;
                    *key_iv_inner.inner.write().await.deref_mut() = Poll::Ready(res);
                    key_iv_inner.notify.notify_waiters();
                });

                Ok(Some(key_iv))
            }
            KeyMethod::None => Ok(None),
            m => Err(anyhow!("unsupported encryption method `{m}`")),
        }
    }

    async fn download_merge(
        mut self,
        client: &Arc<Downloader>,
        parallel_max: usize,
        skip: Option<u64>,
        progress: &indicatif::ProgressBar,
    ) -> Result<TempPath> {
        let (file, path) = tempfile_in(".")
            .await
            .context("failed to create temporary file for merge")?;

        self.set_progress_bar(progress);

        self.map_ok(|seg| seg.download(Arc::clone(client)))
            .try_buffered(parallel_max)
            .try_fold(file, |mut dest, (src, key_iv)| async move {
                let dec = if let Some(key_iv) = key_iv {
                    Some(key_iv.await?)
                } else {
                    None
                };

                decrypt_merge(src, skip, dec, &mut dest).await?;
                progress.inc(1);
                Ok(dest)
            })
            .await?;

        Ok(path)
    }
}

impl Stream for SegmentStream<'_> {
    type Item = Result<Segment>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(ref mut sleep) = self.sleep {
                match sleep.poll_unpin(cx) {
                    Poll::Ready(_) => {
                        self.sleep.take();
                    }
                    Poll::Pending => break Poll::Pending,
                }
            }
            debug_assert!(self.sleep.is_none());

            if let Some(ref mut request) = self.request {
                let playlist = match request.try_poll_unpin(cx) {
                    Poll::Ready(Ok(playlist)) => {
                        self.request.take();
                        playlist
                    }
                    Poll::Ready(Err(e)) => {
                        if self.valid {
                            warn!("{e}");
                            break Poll::Ready(None);
                        } else {
                            break Poll::Ready(Some(Err(e)));
                        }
                    }
                    Poll::Pending => break Poll::Pending,
                };

                let target_duration = playlist.target_duration;
                match self.feed_playlist(playlist) {
                    Err(e) => break Poll::Ready(Some(Err(e))),
                    Ok(0) => {
                        self.sleep = Some(Box::pin(sleep(Duration::from_secs(target_duration))));
                        continue;
                    }
                    Ok(_) => {}
                }
            }
            debug_assert!(self.request.is_none());

            if let Some(ref mut iter) = self.iter {
                match iter.next() {
                    Some(seg) => {
                        self.seq.replace(seg.seq);
                        break Poll::Ready(Some(Ok(seg)));
                    }
                    None if self.end_list => break Poll::Ready(None),
                    _ => {
                        self.iter.take();
                    }
                }
            }
            debug_assert!(self.iter.is_none());

            self.request = Some(self.get_playlist().boxed());
        }
    }
}

struct Segment {
    seq: u64,
    url: Url,
    key_iv: Option<KeyIv>,
}

impl Segment {
    fn new(seq: u64, url: Url, key_iv: Option<KeyIv>) -> Self {
        Self { seq, url, key_iv }
    }

    async fn download(self, client: Arc<Downloader>) -> Result<(TempPath, Option<KeyIv>)> {
        client
            .download(self.url)
            .map_ok(|path| (path, self.key_iv))
            .await
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
