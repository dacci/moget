use anyhow::{anyhow, Context, Result};
use futures::prelude::*;
use indicatif::ProgressBar;
use log::debug;
use reqwest::Url;
use reqwest_middleware::ClientWithMiddleware as Client;
use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tempfile::{NamedTempFile, TempPath};
use tokio::fs::File;
use tokio::io;
use tokio::io::AsyncWriteExt;

pub(crate) struct Downloader {
    client: Client,
    parallel_max: usize,
}

impl Downloader {
    pub fn new(args: &super::Args) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        for header in &args.headers {
            if let Some((k, v)) = header.split_once(": ") {
                let k = k
                    .parse::<reqwest::header::HeaderName>()
                    .map_err(|_| anyhow!("invalid HTTP header name `{k}`"))?;
                let v = v
                    .parse::<reqwest::header::HeaderValue>()
                    .map_err(|_| anyhow!("invalid HTTP header value `{v}`"))?;
                headers.insert(k, v);
            }
        }

        let mut builder = reqwest::Client::builder().default_headers(headers);

        if 0.0 < args.connect_timeout {
            builder = builder.connect_timeout(Duration::from_secs_f64(args.connect_timeout));
        }

        if 0.0 < args.max_time {
            builder = builder.timeout(Duration::from_secs_f64(args.max_time));
        }

        let client = builder.build()?;

        let mut builder = reqwest_middleware::ClientBuilder::new(client);

        if 0 < args.retry {
            builder = builder.with(reqwest_retry::RetryTransientMiddleware::new_with_policy(
                reqwest_retry::policies::ExponentialBackoffBuilder::default()
                    .build_with_max_retries(args.retry),
            ));
        }

        Ok(Self {
            client: builder.build(),
            parallel_max: args.parallel_max,
        })
    }

    pub async fn download_merge(
        self: &Arc<Self>,
        urls: Vec<Url>,
        progress: &ProgressBar,
    ) -> Result<TempPath> {
        let (file, path) = tempfile_in(".")
            .await
            .context("failed to create temporary file for merge")?;

        stream::iter(urls)
            .then(|url| {
                let this = Arc::clone(self);
                #[allow(clippy::async_yields_async)]
                async move {
                    this.download(url)
                }
            })
            .buffered(self.parallel_max)
            .try_fold(file, |mut dest, src| async move {
                Self::merge(src, &mut dest).await?;
                progress.inc(1);
                Ok(dest)
            })
            .await?;

        Ok(path)
    }

    pub async fn download(self: Arc<Self>, url: Url) -> Result<TempPath> {
        let mut res = self
            .client
            .get(url.clone())
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("failed to request to {url}"))?;
        let (mut file, path) = tempfile_in(".")
            .await
            .context("failed to create temporary file for download")?;

        while let Some(bytes) = res
            .chunk()
            .await
            .with_context(|| format!("failed to read from {url}"))?
        {
            file.write_all(&bytes)
                .await
                .with_context(|| format!("failed to write response from {url}"))?;
        }

        debug!(target: "download", "{}", res.url());

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
}

impl Deref for Downloader {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

async fn tempfile_in(dir: impl AsRef<Path>) -> Result<(File, TempPath)> {
    let dir = dir.as_ref().to_path_buf();
    let (file, path) = tokio::task::spawn_blocking(|| NamedTempFile::new_in(dir))
        .await??
        .into_parts();
    Ok((File::from_std(file), path))
}
