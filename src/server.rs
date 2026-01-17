use crate::{Event, FilePart, Metadata, build_socket};
use anyhow::Context;
use cscall::{
    UID_LEN,
    crypto::{Crypto, aes256gcm::Aes256GcmCrypto},
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
    let server = Arc::new(Server::<Aes256GcmCrypto>::new(pwd.as_bytes().into(), socket).await?);
    let fids = Arc::new(DashSet::new());
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
                    let fids = fids.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            handle_request(server_clone, uid, root_clone, event, fids).await
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
    fids: Arc<DashSet<u64>>,
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
                "收到文件请求: {} ({}) [{}, {})",
                req.path,
                req.fid,
                req.start,
                req.end
            );
            if fids.contains(&req.fid) {
                anyhow::bail!("重复的 fid")
            }
            fids.insert(req.fid);
            let _ids_guard = scopeguard::guard((), |_| {
                fids.remove(&req.fid);
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
                    fid: req.fid,
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
