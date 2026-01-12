```rs title="fsend\src\client.rs"
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
    let file_id = OsRng.try_next_u64().context("无法生成随机数")?;
    loop {
        let download_range = match invert(progress.iter(), meta.size).next() {
            None => break,
            Some(r) => r,
        };
        let action_id = OsRng.try_next_u64().context("无法生成随机数")?;
        let event = bitcode::encode(&Event::File(FileRequest {
            path: path_str.clone(),
            start: download_range.start,
            end: download_range.end,
            file_id,
            action_id,
        }));
        loop {
            match client.send(&mut event.clone()).await {
                Err(e) => tracing::warn!("获取文件内容失败：{:?}", e),
                Ok(_) => match get_file(&client, &mut buf, file_id).await {
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
            match tokio::time::timeout(Duration::from_secs(1), get_file(&client, &mut buf, file_id))
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
    pb.finish_with_message("下载完成，等待写入线程");
    drop(tx);
    write_handle.await.context("写入线程异常退出")?;
    Ok(())
}

async fn get_file<C: Crypto>(
    client: &Client<C>,
    buf: &mut Vec<u8>,
    file_id: u64,
) -> anyhow::Result<Option<FilePart>> {
    let res = client.recv(buf).await.context("接收文件内容失败")?;
    if res.is_none() {
        return Ok(None);
    }
    let event: Event = bitcode::decode(buf).context("解码文件内容失败")?;
    match event {
        Event::AckFile(part) if part.file_id == file_id => Ok(Some(part)),
        Event::AckFile(part) => {
            anyhow::bail!(
                "收到错误的 AckFile，期望 file_id: {} 但收到：{}",
                file_id,
                part.file_id
            )
        }
        event => anyhow::bail!("期望收到文件内容 AckFile，但收到：{:?}", event),
    }
}
```

```rs title="fsend\src\lib.rs"
use bitcode::{Decode, Encode};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;

pub mod client;
pub mod progress;
pub mod server;

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
pub enum Event {
    /// 读取元数据 (Path)
    Metadata(String),
    AckMetadata(Metadata),

    /// 读取文件 [start, end) 左闭右开
    File(FileRequest),
    AckFile(FilePart),
    FileEnd,
}

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
pub struct Metadata {
    pub size: u64,
}

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
pub struct FileRequest {
    pub path: String,
    /// 包含 start
    pub start: u64,
    /// 不包含 end
    pub end: u64,
    pub file_id: u64,
    pub action_id: u64,
}

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
pub struct FilePart {
    pub data: Vec<u8>,
    pub start: u64,
    pub file_id: u64,
    pub action_id: u64,
}

pub fn build_socket(addr: SocketAddr) -> std::io::Result<tokio::net::UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    if let Err(e) = socket.set_reuse_address(true) {
        tracing::warn!("Failed to set reuse address: {:?}", e);
    }
    if let Err(e) = socket.set_nonblocking(true) {
        tracing::warn!("Failed to set non-blocking mode: {:?}", e);
    }
    if let Err(e) = socket.set_recv_buffer_size(25 * 1024 * 1024) {
        tracing::warn!("Failed to set receive buffer size: {:?}", e);
    }
    if let Err(e) = socket.set_send_buffer_size(25 * 1024 * 1024) {
        tracing::warn!("Failed to set send buffer size: {:?}", e);
    }
    socket.bind(&addr.into())?;
    tokio::net::UdpSocket::from_std(socket.into())
}
```

```rs title="fsend\src\main.rs"
use clap::{CommandFactory, Parser, Subcommand};
use fsend::{client::handle_connect_mode, server::handle_serve_mode};
use std::path::PathBuf;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser, Debug)]
#[command(name = "fsend")]
#[command(version, about, long_about = None)]
struct Cli {
    /// 客户端模式
    #[arg(short, long)]
    connect: Option<String>,
    /// 文件路径
    #[arg(short, long)]
    path: Option<PathBuf>,

    /// 子命令
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// 启动服务模式
    Serve {
        /// 文件夹路径，默认为当前工作目录
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(true)
        .with_file(true)
        .with_line_number(true)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let cli = Cli::parse();

    if let Some(addr) = cli.connect
        && let Some(path) = cli.path
    {
        // 客户端模式
        handle_connect_mode(&addr, &path).await?;
    } else if let Some(Commands::Serve { path }) = cli.command {
        // 服务端模式
        handle_serve_mode(&path).await?;
    } else {
        Cli::command().print_help()?;
    }

    Ok(())
}
```

```rs title="fsend\src\server.rs"
use crate::{Event, FilePart, Metadata, build_socket};
use anyhow::Context;
use cscall::{
    UID_LEN,
    crypto::{Crypto, nocrypto::NoCrypto},
    server::Server,
};
use dashmap::DashSet;
use mmap_io::{MemoryMappedFile, MmapAdvice, MmapMode};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::fs;

pub async fn handle_serve_mode(serve_root: &Path) -> anyhow::Result<()> {
    let serve_root = fs::canonicalize(&serve_root)
        .await
        .context("无法获取服务目录绝对路径")?;
    let serve_root = Arc::new(serve_root);
    let pwd = rpassword::prompt_password("请输入密码：")?;
    let socket = Arc::new(build_socket("0.0.0.0:8080".parse()?)?);
    let server = Arc::new(Server::<NoCrypto>::new(pwd.as_bytes(), socket).await?);
    let action_ids = Arc::new(DashSet::new());
    tracing::info!("服务器启动，服务目录: {}", serve_root.display());
    let mut buf = Vec::with_capacity(1500);
    loop {
        match server.recv(&mut buf).await {
            Err(e) => tracing::warn!("接收数据失败: {}", e),
            Ok(None) => {}
            Ok(Some((uid, count))) => match bitcode::decode::<Event>(&buf) {
                Err(e) => tracing::warn!(
                    "解码事件失败 (UID: {}, Count: {}): {:?}",
                    hex::encode(uid),
                    count,
                    e
                ),
                Ok(event) => {
                    let server_clone = server.clone();
                    let root_clone = serve_root.clone();
                    let action_ids = action_ids.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            handle_request(server_clone, uid, root_clone, event, action_ids).await
                        {
                            tracing::error!(
                                "处理请求失败 (UID: {}, Count: {}): {:?}",
                                hex::encode(uid),
                                count,
                                e
                            );
                        }
                    });
                }
            },
        }
    }
}

/// 处理单个客户端请求
async fn handle_request<C: Crypto>(
    server: Arc<Server<C>>,
    uid: [u8; UID_LEN],
    root: Arc<PathBuf>,
    event: Event,
    action_ids: Arc<DashSet<u64>>,
) -> anyhow::Result<()> {
    let client = server.get(&uid).await.context("找不到客户端连接")?;
    match event {
        Event::Metadata(req_path_str) => {
            tracing::info!("收到元数据请求: {}", req_path_str);
            let path = secure_resolve_path(&root, &req_path_str).await?;
            let meta = fs::metadata(&path).await.context("无法读取文件元数据")?;
            if !meta.is_file() {
                anyhow::bail!("请求的路径不是一个文件");
            }
            let response = Event::AckMetadata(Metadata { size: meta.len() });
            let mut send_buf = bitcode::encode(&response);
            client.send(&mut send_buf).await.context("发送元数据失败")?;
        }
        Event::File(req) => {
            tracing::info!(
                "收到文件请求: {} ({})({}) [{}, {})",
                req.path,
                req.file_id,
                req.action_id,
                req.start,
                req.end
            );
            if action_ids.contains(&req.action_id) {
                anyhow::bail!("重复的 action_id")
            }
            action_ids.insert(req.action_id);
            let _ids_guard = scopeguard::guard((), |_| {
                action_ids.remove(&req.action_id);
            });
            let path = secure_resolve_path(&root, &req.path).await?;
            let file_size = fs::metadata(&path).await?.len();
            if file_size == 0 {
                return Ok(());
            }
            let mmap = tokio::task::spawn_blocking(move || {
                MemoryMappedFile::builder(path)
                    .mode(MmapMode::ReadOnly)
                    .open()
            })
            .await??;
            let mut remaining = req.end.saturating_sub(req.start).min(file_size);
            let mut curr_pos = req.start;
            mmap.advise(curr_pos, remaining, MmapAdvice::Sequential)?;
            const CHUNK_SIZE: u64 = 1024;
            while remaining > 0 {
                let len = CHUNK_SIZE.min(remaining);
                let part = FilePart {
                    start: curr_pos,
                    data: mmap
                        .as_slice(curr_pos, len)
                        .context("无法读取文件")?
                        .to_vec(),
                    file_id: req.file_id,
                    action_id: req.action_id,
                };
                let response = Event::AckFile(part);
                while let Err(e) = client.send(&mut bitcode::encode(&response)).await {
                    tracing::warn!("发送文件块失败 (offset {}): {:?}", curr_pos, e);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                remaining -= len;
                curr_pos += len;
            }
        }
        _ => tracing::debug!("收到未处理的事件类型: {:?}", event),
    }
    Ok(())
}

/// 安全路径解析：防止目录遍历攻击 (../../)
async fn secure_resolve_path(root: &Path, req_path: &str) -> anyhow::Result<PathBuf> {
    let req_path = req_path.trim_start_matches('/');
    let full_path = root.join(req_path);
    let canonical_path = fs::canonicalize(full_path)
        .await
        .context("路径不存在或无效")?;
    if !canonical_path.starts_with(root) {
        anyhow::bail!("非法访问：试图访问服务目录之外的文件");
    }
    Ok(canonical_path)
}
```

```rs title="fsend\src\progress\invert.rs"
use crate::progress::ProgressEntry;
use std::slice::Iter;

pub struct InvertIter<'a> {
    iter: Iter<'a, ProgressEntry>,
    prev_end: u64,
    total_size: u64,
}

impl<'a> Iterator for InvertIter<'a> {
    type Item = ProgressEntry;

    fn next(&mut self) -> Option<Self::Item> {
        for range in self.iter.by_ref() {
            if range.start > self.prev_end {
                let gap = self.prev_end..range.start;
                self.prev_end = range.end;
                return Some(gap);
            }
            self.prev_end = range.end;
        }
        if self.prev_end < self.total_size {
            let gap = self.prev_end..self.total_size;
            self.prev_end = self.total_size;
            return Some(gap);
        }
        None
    }
}

pub fn invert(progress: Iter<ProgressEntry>, total_size: u64) -> InvertIter {
    InvertIter {
        iter: progress,
        prev_end: 0,
        total_size,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::single_range_in_vec_init)]
    use super::*;

    // 辅助函数，方便测试时将迭代器转为 Vec 比较
    fn invert_vec(progress: &[ProgressEntry], total_size: u64) -> Vec<ProgressEntry> {
        invert(progress.iter(), total_size).collect()
    }

    #[test]
    fn test_reverse_progress() {
        assert_eq!(invert_vec(&[], 10), [0..10]);
        assert_eq!(invert_vec(&[0..5], 10), [5..10]);
        assert_eq!(invert_vec(&[5..10], 10), [0..5]);
        assert_eq!(invert_vec(&[0..5, 7..10], 10), [5..7]);
        assert_eq!(invert_vec(&[0..3, 5..8], 10), [3..5, 8..10]);
        assert_eq!(invert_vec(&[1..3, 5..8], 10), [0..1, 3..5, 8..10]);
    }
}
```

```rs title="fsend\src\progress\merge.rs"
use crate::progress::{Mergeable, ProgressEntry};

pub trait Merge {
    fn merge_progress(&mut self, new: ProgressEntry);
}

impl Merge for Vec<ProgressEntry> {
    fn merge_progress(&mut self, new: ProgressEntry) {
        let i = self.partition_point(|old| old.start < new.start);
        if i == self.len() {
            match self.last_mut() {
                Some(last) if last.end == new.start => {
                    last.end = new.end;
                }
                _ => self.push(new),
            }
        } else {
            let u1 = i > 0 && self[i - 1].can_merge(&new);
            let u2 = self[i].can_merge(&new);
            if u1 && u2 {
                self[i - 1].end = self[i].end;
                self.remove(i);
            } else if u1 {
                self[i - 1].end = new.end;
            } else if u2 {
                self[i].start = new.start;
            } else {
                self.insert(i, new);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_into_empty_vec() {
        let mut v: Vec<ProgressEntry> = Vec::new();
        v.merge_progress(10..20);
        assert_eq!(v, vec![10..20]);
    }

    #[test]
    fn test_append_non_overlapping() {
        #[allow(clippy::single_range_in_vec_init)]
        let mut v = vec![1..5];
        v.merge_progress(6..10);
        assert_eq!(v, vec![1..5, 6..10]);
    }

    #[test]
    fn test_prepend_non_overlapping() {
        #[allow(clippy::single_range_in_vec_init)]
        let mut v = vec![6..10];
        v.merge_progress(1..5);
        assert_eq!(v, vec![1..5, 6..10]);
    }

    #[test]
    fn test_merge_with_last() {
        #[allow(clippy::single_range_in_vec_init)]
        let mut v = vec![1..5];
        v.merge_progress(5..10);
        assert_eq!(v, vec![1..10]);
    }

    #[test]
    fn test_insert_between_two() {
        let mut v = vec![1..5, 10..15];
        v.merge_progress(6..8);
        assert_eq!(v, vec![1..5, 6..8, 10..15]);
    }

    #[test]
    fn test_merge_adjacent() {
        #[allow(clippy::single_range_in_vec_init)]
        let mut v = vec![1..5];
        v.merge_progress(5..10);
        assert_eq!(v, vec![1..10]);
    }
}
```

```rs title="fsend\src\progress\mod.rs"
use core::ops::Range;

pub mod invert;
pub mod merge;

pub type ProgressEntry = Range<u64>;

pub trait Mergeable {
    fn can_merge(&self, other: &Self) -> bool;
    fn contain(&self, other: &Self) -> bool;
}

impl Mergeable for ProgressEntry {
    #[inline(always)]
    fn can_merge(&self, b: &Self) -> bool {
        self.start == b.end || b.start == self.end
    }

    #[inline(always)]
    fn contain(&self, other: &Self) -> bool {
        other.start >= self.start && other.end <= self.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_can_merge_adjacent() {
        let a = 0..5;
        let b = 5..10;
        assert!(a.can_merge(&b));
        assert!(b.can_merge(&a));
    }

    #[test]
    fn test_cannot_merge_non_adjacent_non_overlapping() {
        let a = 0..5;
        let b = 7..10;
        assert!(!a.can_merge(&b));
        assert!(!b.can_merge(&a));
    }

    #[test]
    fn test_cannot_merge_disjoint() {
        let a = 0..5;
        let b = 6..15;
        assert!(!a.can_merge(&b));
        assert!(!b.can_merge(&a));
    }
}
```

