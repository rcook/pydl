use anyhow::{Context, Result};
use futures_util::{StreamExt, TryStreamExt};
use reqwest::Response;
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio_util::io::ReaderStream;

use crate::ByteStream;
use crate::entry::EntryPaths;

pub async fn open_stream(path: &std::path::Path) -> Result<ByteStream> {
    let file = File::open(path)
        .await
        .with_context(|| format!("opening cache body {}", path.display()))?;
    let s = ReaderStream::new(file).map(|r| r.map_err(anyhow::Error::from));
    Ok(Box::pin(s))
}

pub fn passthrough_stream(resp: Response) -> ByteStream {
    Box::pin(resp.bytes_stream().map_err(anyhow::Error::from))
}

pub async fn download_to_tmp(paths: &EntryPaths, resp: Response) -> Result<()> {
    let tmp = paths.tmp_body();
    match stream_response_to_path(resp, &tmp).await {
        Ok(()) => tokio::fs::rename(&tmp, &paths.body)
            .await
            .with_context(|| format!("renaming {} -> {}", tmp.display(), paths.body.display())),
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

async fn stream_response_to_path(resp: Response, dest: &std::path::Path) -> Result<()> {
    let file = File::create(dest)
        .await
        .with_context(|| format!("creating {}", dest.display()))?;
    let mut writer = BufWriter::new(file);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading upstream body")?;
        writer
            .write_all(&chunk)
            .await
            .context("writing cache body")?;
    }
    writer.flush().await.context("flushing cache body")?;
    Ok(())
}

pub async fn collect_stream(mut stream: ByteStream) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
    }
    Ok(buf)
}
