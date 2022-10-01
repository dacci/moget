use anyhow::Result;
use futures::prelude::*;
use log::info;
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
                headers.insert(k.parse::<reqwest::header::HeaderName>()?, v.parse()?);
            }
        }

        let mut builder = reqwest::Client::builder().default_headers(headers);

        if let Some(secs) = args.connect_timeout {
            builder = builder.connect_timeout(Duration::from_secs_f64(secs));
        }

        if let Some(secs) = args.max_time {
            builder = builder.timeout(Duration::from_secs_f64(secs));
        }

        let client = builder.build()?;

        let mut builder = reqwest_middleware::ClientBuilder::new(client);

        if let Some(n) = args.retry {
            builder = builder.with(reqwest_retry::RetryTransientMiddleware::new_with_policy(
                reqwest_retry::policies::ExponentialBackoffBuilder::default()
                    .build_with_max_retries(n),
            ));
        }

        Ok(Self {
            client: builder.build(),
            parallel_max: args.parallel_max,
        })
    }

    pub async fn download_merge(self: &Arc<Self>, urls: Vec<Url>) -> Result<TempPath> {
        let (file, path) = tempfile_in(".").await?;

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
                Ok(dest)
            })
            .await?;

        Ok(path)
    }

    pub async fn download(self: Arc<Self>, url: Url) -> Result<TempPath> {
        let mut res = self.client.get(url).send().await?.error_for_status()?;
        let (mut file, path) = tempfile_in(".").await?;

        while let Some(bytes) = res.chunk().await? {
            file.write_all(&bytes).await?;
        }

        info!("{}", res.url());

        Ok(path)
    }

    async fn merge(
        src: impl AsRef<Path>,
        dest: &mut (impl io::AsyncWrite + Unpin + ?Sized),
    ) -> Result<()> {
        let mut src = File::open(src).await?;
        io::copy(&mut src, dest).await?;
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
