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
