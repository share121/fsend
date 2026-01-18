use crate::{
    Event, FilePart, FileRequest, build_socket,
    progress::{ProgressEntry, Total, invert::invert, merge::Merge},
};
use anyhow::Context;
use cscall::{
    CsError,
    client::{Client, ClientConfig},
    crypto::{Crypto, aes256gcm::Aes256GcmCrypto},
};
use indicatif::{ProgressBar, ProgressStyle};
use mmap_io::{MemoryMappedFile, MmapMode};
use rand::{RngCore, rngs::OsRng};
use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};

pub async fn handle_connect_mode(addr: &str, path: &Path) -> anyhow::Result<()> {
    let addr: SocketAddr = addr.parse()?;
    let filename = path.file_name().context("获取文件名失败")?;
    let pwd = rpassword::prompt_password("请输入密码：")?;
    let socket = Arc::new(build_socket("0.0.0.0:0".parse()?)?);
    let client = tokio::time::timeout(
        Duration::from_secs(30),
        Client::<Aes256GcmCrypto>::new(ClientConfig {
            socket,
            target: addr,
            ttl: Duration::from_secs(10),
            pwd: pwd.as_bytes().into(),
        }),
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
            Ok(_) => match client.recv_timeout(&mut buf, Duration::from_secs(10)).await {
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
        .huge_pages(true)
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
    let file_id = OsRng.next_u64();
    'outer: loop {
        let download_ranges: Vec<_> = invert(progress.iter(), meta.size, 1024 * 1024)
            .take(64)
            .map(|r| (r.start, r.end))
            .collect();
        if download_ranges.is_empty() {
            break;
        }
        let action_id = OsRng.next_u64();
        let event = bitcode::encode(&Event::File(FileRequest {
            path: path_str.clone(),
            ranges: download_ranges,
            file_id,
            action_id,
        }));
        loop {
            match client.send(&mut event.clone()).await {
                Err(e) => tracing::warn!("获取文件内容失败：{:?}", e),
                Ok(_) => match get_file(&client, &mut buf, file_id, Duration::from_secs(5)).await {
                    Err(e) => tracing::warn!("获取文件内容失败：{:?}", e),
                    Ok(Some(Event::AckFile(part))) => {
                        let old_total = progress.total();
                        progress.merge_progress(part.start..part.start + part.data.len() as u64);
                        let new_total = progress.total();
                        pb.inc(new_total - old_total);
                        tx.send(part).context("写入线程异常退出")?;
                        break;
                    }
                    Ok(Some(Event::FileEnd(_)))
                        if invert(progress.iter(), meta.size, 0).next().is_none() =>
                    {
                        break 'outer;
                    }
                    Ok(_) => {}
                },
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        loop {
            match get_file(&client, &mut buf, file_id, Duration::from_secs(1)).await {
                Err(CsError::RecvTimeout(_)) => {
                    tracing::warn!("获取文件内容超时");
                    break;
                }
                Err(e) => tracing::warn!("获取文件内容失败：{:?}", e),
                Ok(Some(Event::AckFile(part))) => {
                    let old_total = progress.total();
                    progress.merge_progress(part.start..part.start + part.data.len() as u64);
                    let new_total = progress.total();
                    pb.inc(new_total - old_total);
                    tx.send(part).context("写入线程异常退出")?;
                }
                Ok(Some(Event::FileEnd(_))) => {
                    break;
                }
                Ok(_) => {}
            }
        }
    }
    pb.finish();
    tracing::info!("下载完成，等待写入线程");
    drop(tx);
    write_handle.await.context("写入线程异常退出")?;
    tracing::info!("写入完成");
    Ok(())
}

async fn get_file<C: Crypto>(
    client: &Client<C>,
    buf: &mut Vec<u8>,
    file_id: u64,
    timeout: Duration,
) -> Result<Option<Event>, CsError> {
    let res = client.recv_timeout(buf, timeout).await?;
    if res.is_none() {
        return Ok(None);
    }
    let event: Event = bitcode::decode(buf).or(Err(CsError::InvalidFormat))?;
    match event {
        Event::AckFile(part) if part.file_id == file_id => Ok(Some(Event::AckFile(part))),
        Event::FileEnd(recv_file_id) if recv_file_id == file_id => {
            Ok(Some(Event::FileEnd(recv_file_id)))
        }
        Event::FileEnd(recv_file_id) => {
            tracing::warn!(
                "收到错误的 FileEnd，期望 file_id: {:x} 但收到：{:x}",
                file_id,
                recv_file_id
            );
            Err(CsError::InvalidFormat)
        }
        Event::AckFile(part) => {
            tracing::warn!(
                "收到错误的 AckFile，期望 file_id: {:x} 但收到：{:x}",
                file_id,
                part.file_id
            );
            Err(CsError::InvalidFormat)
        }
        event => {
            tracing::warn!("期望收到文件内容 AckFile，但收到：{:?}", event);
            Err(CsError::InvalidFormat)
        }
    }
}
