use std::net::SocketAddr;

use bitcode::{Decode, Encode};
use socket2::{Domain, Protocol, Socket, Type};

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
    pub fid: u64,
}

#[derive(Encode, Decode, PartialEq, Debug, Clone)]
pub struct FilePart {
    pub data: Vec<u8>,
    pub start: u64,
    pub fid: u64,
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
