use crate::{
    Event, FilePart, FileRequest, build_socket,
    progress::{Mergeable, ProgressEntry, invert::invert, merge::Merge},
};
use anyhow::Context;
use cscall::{
    client::Client,
    crypto::{Crypto, nocrypto::NoCrypto},
};
use indicatif::{ProgressBar, ProgressStyle};
use mmap_io::{MemoryMappedFile, MmapMode, flush::FlushPolicy};
use rand::{TryRngCore, rngs::OsRng};
use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};

pub async fn handle_connect_mode(addr: &str, path: &Path) -> anyhow::Result<()> {
    let addr: SocketAddr = addr.parse()?;
    let filename = path.file_name().context("获取文件名失败")?;
    let pwd = rpassword::prompt_password("请输入密码：")?;
    let socket = Arc::new(build_socket("0.0.0.0:0".parse()?)?);
    let client = tokio::time::timeout(
        Duration::from_secs(10),
        Client::<NoCrypto>::new(pwd.into_bytes(), addr, socket),
    )
    .await
    .context("连接超时")?
    .context("连接失败")?;
    let mut buf = Vec::with_capacity(1500);
    let path_str = path.to_string_lossy().to_string();
    let event = bitcode::encode(&Event::Metadata(path_str.clone()));
    let meta = loop {
        match client.send(&mut event.clone()).await {
            Err(e) => tracing::warn!("获取元数据失败：{:?}", e),
            Ok(_) => match client.recv(&mut buf).await {
                Err(e) => tracing::warn!("接收元数据失败：{:?}", e),
                Ok(None) => {}
                Ok(Some(_)) => match bitcode::decode::<Event>(&buf) {
                    Err(e) => tracing::warn!("解码元数据失败：{:?}", e),
                    Ok(event) => match event {
                        Event::AckMetadata(metadata) => break metadata,
                        event => tracing::warn!("期望收到元数据 AckMetadata，但收到：{:?}", event),
                    },
                },
            },
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let mmap = MemoryMappedFile::builder(filename)
        .mode(MmapMode::ReadWrite)
        .size(meta.size)
        .flush_policy(FlushPolicy::EveryBytes(256 * 1024))
        .create()?;
    let (tx, rx) = crossfire::spsc::unbounded_blocking::<FilePart>();
    let write_handle = tokio::task::spawn_blocking(move || {
        while let Ok(file_part) = rx.recv() {
            while let Err(e) = mmap.update_region(file_part.start, &file_part.data) {
                tracing::warn!(
                    "写入文件块 [{}, {}) 失败：{:?}",
                    file_part.start,
                    file_part.start + file_part.data.len() as u64,
                    e
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        while let Err(e) = mmap.flush() {
            tracing::warn!("刷新文件内容失败：{:?}", e);
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let mut progress: Vec<ProgressEntry> = Vec::new();
    let pb = ProgressBar::new(meta.size);
    pb.set_style(
            ProgressStyle::with_template(
                // 样式：旋转器 [耗时] [进度条] 字节/总字节 (预计剩余时间)
                "{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({binary_bytes_per_sec} {eta})",
            )
            .unwrap()
            .progress_chars("#>-"),
        );
    loop {
        let download_range = match invert(progress.iter(), meta.size).next() {
            None => break,
            Some(r) => r,
        };
        let fid = OsRng.try_next_u64().context("无法生成随机数")?;
        let event = bitcode::encode(&Event::File(FileRequest {
            path: path_str.clone(),
            start: download_range.start,
            end: download_range.end,
            fid,
        }));
        loop {
            match client.send(&mut event.clone()).await {
                Err(e) => tracing::warn!("获取文件内容失败：{:?}", e),
                Ok(_) => match get_file(&client, &mut buf, fid).await {
                    Err(e) => tracing::warn!("获取文件内容失败：{:?}", e),
                    Ok(None) => {}
                    Ok(Some(part)) => {
                        progress.merge_progress(part.start..part.start + part.data.len() as u64);
                        pb.inc(part.data.len() as u64);
                        tx.send(part).context("写入线程异常退出")?;
                        break;
                    }
                },
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        loop {
            if progress.iter().any(|p| p.contain(&download_range)) {
                break;
            }
            match tokio::time::timeout(Duration::from_secs(1), get_file(&client, &mut buf, fid))
                .await
            {
                Err(e) => {
                    tracing::warn!("获取文件内容超时：{:?}", e);
                    break;
                }
                Ok(Err(e)) => tracing::warn!("获取文件内容失败：{:?}", e),
                Ok(Ok(Some(part))) => {
                    progress.merge_progress(part.start..part.start + part.data.len() as u64);
                    pb.inc(part.data.len() as u64);
                    tx.send(part).context("写入线程异常退出")?;
                }
                Ok(Ok(None)) => {}
            }
        }
    }
    pb.finish();
    tracing::info!("下载完成，等待写入线程");
    drop(tx);
    write_handle.await.context("写入线程异常退出")?;
    Ok(())
}

async fn get_file<C: Crypto>(
    client: &Client<C>,
    buf: &mut Vec<u8>,
    fid: u64,
) -> anyhow::Result<Option<FilePart>> {
    let res = client.recv(buf).await.context("接收文件内容失败")?;
    if res.is_none() {
        return Ok(None);
    }
    let event: Event = bitcode::decode(buf).context("解码文件内容失败")?;
    match event {
        Event::AckFile(part) if part.fid == fid => Ok(Some(part)),
        Event::AckFile(part) => {
            anyhow::bail!("收到错误的 AckFile，期望 fid: {} 但收到：{}", fid, part.fid)
        }
        event => anyhow::bail!("期望收到文件内容 AckFile，但收到：{:?}", event),
    }
}
