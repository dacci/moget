use anyhow::{anyhow, bail, Context, Result};
use futures::prelude::*;
use reqwest::{IntoUrl, Proxy, Url};
use reqwest_middleware::ClientWithMiddleware as Client;
use std::borrow::Cow;
use std::iter::StepBy;
use std::ops::{Deref, Range};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tempfile::{NamedTempFile, TempPath};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tracing::debug;

pub(crate) struct Downloader {
    client: Client,
}

impl Downloader {
    pub fn new(args: &super::Args) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        for header in &args.headers {
            if let Some((k, v)) = header.split_once(':') {
                let k = k
                    .trim()
                    .parse::<reqwest::header::HeaderName>()
                    .map_err(|_| anyhow!("invalid HTTP header name `{k}`"))?;
                let v = v
                    .trim()
                    .parse::<reqwest::header::HeaderValue>()
                    .map_err(|_| anyhow!("invalid HTTP header value `{v}`"))?;
                headers.insert(k, v);
            }
        }

        let mut builder = reqwest::Client::builder().default_headers(headers);

        if let Some(proxy) = build_proxy(args.proxy.as_deref(), args.proxy_user.as_deref())? {
            builder = builder.proxy(proxy);
        }

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
        })
    }

    pub fn get_bytes(
        &self,
        url: impl IntoUrl,
    ) -> impl Future<Output = reqwest_middleware::Result<bytes::Bytes>> {
        self.get(url)
            .send()
            .and_then(|r| async { r.error_for_status() }.err_into())
            .and_then(|r| r.bytes().err_into())
    }

    pub async fn download(self: Arc<Self>, url: Url) -> Result<TempPath> {
        let mut res = self
            .client
            .get(url.clone())
            .send()
            .and_then(|r| async { r.error_for_status() }.err_into())
            .await
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
}

impl Deref for Downloader {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

fn build_proxy(url: Option<&str>, auth: Option<&str>) -> Result<Option<Proxy>> {
    let url = match url {
        None => return Ok(None),
        Some(url) => url,
    };

    let mut proxy =
        Proxy::all(url).with_context(|| anyhow!("failed to configure proxy from URL `{url}`"))?;

    // user info of the URL takes precedence, as in cURL.
    let auth = match url.parse::<Url>() {
        Ok(url) if !url.username().is_empty() || url.password().is_some() => Some((
            Cow::Owned(url.username().into()),
            Cow::Owned(url.password().unwrap_or_default().into()),
        )),
        _ if auth.is_some() => match auth.unwrap().split_once(':') {
            None => bail!("illegal proxy authentication `{}`", auth.unwrap()),
            Some((username, password)) => Some((Cow::Borrowed(username), Cow::Borrowed(password))),
        },
        _ => None,
    };

    if let Some((username, password)) = auth {
        proxy = proxy.basic_auth(&username, &password);
    }

    Ok(Some(proxy))
}

pub async fn tempfile_in(dir: impl AsRef<Path>) -> Result<(File, TempPath)> {
    let dir = dir.as_ref().to_path_buf();
    let (file, path) = tokio::task::spawn_blocking(|| NamedTempFile::new_in(dir))
        .await??
        .into_parts();
    Ok((File::from_std(file), path))
}

pub struct SplitBy<'a> {
    src: &'a str,
    by: usize,
    iter: StepBy<Range<usize>>,
}

impl<'a> SplitBy<'a> {
    pub fn new(src: &'a str, by: usize) -> Self {
        Self {
            src,
            by,
            iter: (0..src.len()).step_by(by),
        }
    }
}

impl<'a> Iterator for SplitBy<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        match self.iter.next() {
            None => None,
            Some(pos) => Some(&self.src[pos..self.src.len().min(pos + self.by)]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_by() {
        let mut iter = SplitBy::new("abc", 2);
        assert_eq!(iter.next(), Some("ab"));
        assert_eq!(iter.next(), Some("c"));
        assert_eq!(iter.next(), None);
    }
}
