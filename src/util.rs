use crate::Error;
use anyhow::{Context as _, anyhow, bail};
use futures::prelude::*;
use reqwest::{Client, Proxy, Url};
use std::borrow::Cow;
use std::iter::StepBy;
use std::ops::{Deref, Range};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tempfile::{NamedTempFile, TempPath};
use tokio::fs::File;
use tokio::io::{AsyncWriteExt as _, BufWriter};
use tower::retry::backoff::{ExponentialBackoff, ExponentialBackoffMaker};
use tower::util::BoxCloneService;
use tower::util::rng::HasherRng;

#[derive(Clone)]
struct ExponentialBackoffPolicy {
    backoff: ExponentialBackoff,
    max_retries: u32,
    tries: u32,
}

impl ExponentialBackoffPolicy {
    fn new(backoff: ExponentialBackoff, max_retries: u32) -> Self {
        Self {
            backoff,
            max_retries,
            tries: 0,
        }
    }
}

impl<Req: Clone, Res> tower::retry::Policy<Req, Res, Error> for ExponentialBackoffPolicy {
    type Future = tokio::time::Sleep;

    fn retry(&mut self, _: &mut Req, result: &mut Result<Res, Error>) -> Option<Self::Future> {
        use tower::retry::backoff::Backoff;

        if let Err(Error::Reqwest(_)) = result {
            if self.tries < self.max_retries {
                self.tries += 1;
                Some(self.backoff.next_backoff())
            } else {
                None
            }
        } else {
            None
        }
    }

    fn clone_request(&mut self, req: &Req) -> Option<Req> {
        Some(req.clone())
    }
}

pub(crate) struct Downloader {
    client: Arc<Client>,
    service: BoxCloneService<(Arc<Client>, Url), TempPath, Error>,
}

impl Downloader {
    pub fn new(args: &super::Args) -> anyhow::Result<Self> {
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
        if let Some(cookie) = &args.cookie {
            headers.insert(
                reqwest::header::COOKIE,
                cookie
                    .parse()
                    .map_err(|_| anyhow!("invalid cookie value `{cookie}`"))?,
            );
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

        let client = builder.build().map(Arc::new)?;

        use tower::retry::backoff::MakeBackoff;
        let backoff = ExponentialBackoffMaker::new(
            Duration::from_secs(1),
            Duration::from_mins(30),
            0.5,
            HasherRng::new(),
        )?
        .make_backoff();

        Ok(Self {
            client,
            service: BoxCloneService::new(
                tower::ServiceBuilder::new()
                    .retry(ExponentialBackoffPolicy::new(backoff, args.retry))
                    .service_fn(Self::download_service),
            ),
        })
    }

    async fn download_service((client, url): (Arc<Client>, Url)) -> Result<TempPath, Error> {
        let (file, path) = tempfile_in(".").await?;
        let mut file = BufWriter::new(file);

        let mut res = client.get(url).send().await?.error_for_status()?;
        while let Some(bytes) = res.chunk().await? {
            file.write_all(&bytes).await?;
        }
        file.flush().await?;

        Ok(path)
    }

    pub fn download(&self, url: Url) -> impl Future<Output = Result<TempPath, Error>> {
        use tower::Service;

        let client = Arc::clone(&self.client);
        self.service.clone().call((client, url))
    }
}

impl Deref for Downloader {
    type Target = Arc<Client>;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

fn build_proxy(url: Option<&str>, auth: Option<&str>) -> anyhow::Result<Option<Proxy>> {
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

pub async fn tempfile_in(dir: impl AsRef<Path>) -> Result<(File, TempPath), Error> {
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
