//! `nai-gate`：猫头鹰公益站网关。
//!
//! - `nai-gate serve`：启动网关（默认）；
//! - `nai-gate healthcheck`：容器健康检查（distroless 里没有 curl），访问本机 /healthz。

use clap::{Parser, Subcommand};
use gate_server::env::EnvConfig;
use gate_server::web::Assets;
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "nai-gate", version, about = "猫头鹰公益站网关")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 启动网关
    Serve {
        #[arg(long, env = "BIND", default_value = "0.0.0.0:8000")]
        bind: SocketAddr,
    },
    /// 健康检查：GET http://127.0.0.1:<port>/healthz，非 200 退出码 1
    Healthcheck {
        #[arg(long, env = "PORT", default_value_t = 8000)]
        port: u16,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Serve {
        bind: "0.0.0.0:8000".parse()?,
    }) {
        Cmd::Serve { bind } => serve(bind),
        Cmd::Healthcheck { port } => healthcheck(port),
    }
}

fn healthcheck(port: u16) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(4),
    )?;
    s.set_read_timeout(Some(Duration::from_secs(4)))?;
    s.write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")?;
    let mut buf = String::new();
    s.read_to_string(&mut buf)?;
    if buf.starts_with("HTTP/1.1 200") {
        Ok(())
    } else {
        std::process::exit(1)
    }
}

fn serve(bind: SocketAddr) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let env = EnvConfig::from_process();
        std::fs::create_dir_all(&env.data_dir)?;
        let db = gate_db::Db::open(
            env.db_path(),
            gate_server::time::now(),
            gate_core::reasons::BACKFILL,
        )?;
        let assets = Assets::new(std::env::var_os("STATIC_DIR").map(Into::into));
        let state = gate_server::AppState::new(env, db, assets);
        let app = gate_server::app(state);
        let listener = tokio::net::TcpListener::bind(bind).await?;
        tracing::info!(%bind, version = gate_server::state::VERSION, "猫头鹰公益站网关已启动");
        axum::serve(
            listener,
            axum::ServiceExt::<axum::extract::Request>::into_make_service_with_connect_info::<
                SocketAddr,
            >(app),
        )
        .with_graceful_shutdown(shutdown_signal())
        .await?;
        anyhow::Ok(())
    })
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("收到停止信号，开始优雅关停");
}
