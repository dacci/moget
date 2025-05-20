use anyhow::{Context, Error, Result, anyhow, bail};
use futures::prelude::*;
use reqwest::{Client, IntoUrl, Proxy, Url};
use reqwest_middleware::ClientWithMiddleware;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::{RetryDecision, RetryPolicy};
use std::borrow::Cow;
use std::iter::StepBy;
use std::ops::{Deref, Range};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tempfile::{NamedTempFile, TempPath};
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};

pub(crate) struct Downloader {
    client: ClientWithMiddleware,
    retry_policy: Option<ExponentialBackoff>,
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

        let mut builder = Client::builder().default_headers(headers);

        if let Some(proxy) = build_proxy(args.proxy.as_deref(), args.proxy_user.as_deref())? {
            builder = builder.proxy(proxy);
        }

        if 0.0 < args.connect_timeout {
            builder = builder.connect_timeout(Duration::from_secs_f64(args.connect_timeout));
        }

        if 0.0 < args.max_time {
            builder = builder.timeout(Duration::from_secs_f64(args.max_time));
        }

        let raw_client = builder.build()?;

        let mut builder = reqwest_middleware::ClientBuilder::new(raw_client);

        let retry_policy = if 0 < args.retry {
            let retry_policy = reqwest_retry::policies::ExponentialBackoffBuilder::default()
                .build_with_max_retries(args.retry);
            builder = builder.with(reqwest_retry::RetryTransientMiddleware::new_with_policy(
                retry_policy,
            ));
            Some(retry_policy)
        } else {
            None
        };

        Ok(Self {
            client: builder.build(),
            retry_policy,
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
        let request = self.client.get(url.clone()).build()?;

        let mut n_past_retries = 0;
        let start_time = SystemTime::now();
        loop {
            let e = match self.client.execute(request.try_clone().unwrap()).await {
                Ok(res) => {
                    let mut res = res
                        .error_for_status()
                        .with_context(|| format!("failed to request to {url}"))?;

                    let (file, path) = tempfile_in(".")
                        .await
                        .context("failed to create temporary file for download")?;
                    let mut file = BufWriter::new(file);

                    loop {
                        match res.chunk().await {
                            Ok(None) => {
                                file.flush().await?;
                                return Ok(path);
                            }
                            Ok(Some(bytes)) => file
                                .write_all(&bytes)
                                .await
                                .with_context(|| format!("failed to write response from {url}"))?,
                            Err(e) => {
                                break Error::new(e).context(format!("failed to read from {url}"));
                            }
                        }
                    }
                }
                Err(e) => Error::new(e).context(format!("failed to request to {url}")),
            };

            let Some(ref policy) = self.retry_policy else {
                return Err(e);
            };

            match policy.should_retry(start_time, n_past_retries) {
                RetryDecision::Retry { execute_after } => {
                    let duration = execute_after
                        .duration_since(SystemTime::now())
                        .unwrap_or_default();
                    tracing::warn!(
                        "Retry attempt #{}. Sleeping {:?} before the next attempt",
                        n_past_retries,
                        duration
                    );
                    tokio::time::sleep(duration).await;
                    n_past_retries += 1;
                }
                RetryDecision::DoNotRetry => return Err(e),
            };
        }
    }
}

impl Deref for Downloader {
    type Target = ClientWithMiddleware;

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
