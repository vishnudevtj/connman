use std::{
    fs::File,
    io::BufReader,
    path::{Path, PathBuf}, time::Duration,
};

use anyhow::anyhow;
use argh::FromArgs;

mod connman;
mod docker;
mod id;
mod proxy;
mod server;
mod tui;

use connman::ConnmanBuilder;

use fern::Dispatch;
use log::{info, LevelFilter};

use tokio::time::sleep;
use tokio_rustls::rustls::{Certificate, PrivateKey};

use webpki::EndEntityCert;

#[derive(FromArgs)]
/// Auto start docker container on request.
struct ConnmanArg {
    /// host address for starting gRPC server
    #[argh(option, short = 'h', default = "String::from(\"0.0.0.0\")")]
    host: String,

    /// on which address to listen for proxying requests. This value is returns as
    /// url for TCP connection type
    #[argh(option)]
    proxy_host: String,

    /// host port on which to start gRPC server
    #[argh(option, short = 'p', default = "50051")]
    port: u16,

    /// address of docker HTTP API server   
    #[argh(option)]
    docker_host: String,

    /// port on which docker HTTP API server
    /// listens
    #[argh(option)]
    docker_port: u16,

    /// cert file
    #[argh(option, short = 'c')]
    cert: Option<PathBuf>,

    /// key file
    #[argh(option)]
    key: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Setup TUI
    tui::tui();

    // \_(ツ)_/
    sleep(Duration::from_secs(2)).await;
    // Since TUI contains logging we are not enabling stdout logging
    // Enable this if TUI is turned off.
    // _setup_logger()?;

    let arg: ConnmanArg = argh::from_env();
    start_grpc(arg).await
}

async fn start_grpc(arg: ConnmanArg) -> anyhow::Result<()> {
    let proxy_host = arg.proxy_host.parse()?;
    let mut connman = ConnmanBuilder::new(proxy_host);

    if let (Some(c), Some(k)) = (arg.cert, arg.key) {
        info!("Loading Certificate and Private Key");
        let mut tls_host = String::new();
        let cert = load_certificates_from_pem(&c)?;
        let key = load_private_key_from_file(&k)?;
        for c in cert.iter() {
            let cert_der = c.as_ref();
            let cert = EndEntityCert::try_from(cert_der)?;
            for dns_name in cert.dns_names()? {
                let name: &str = dns_name.into();
                if let Some(host) = name.strip_prefix("*.") {
                    info!("Got a Wild Card Certificate for DNS Host: {}", host);
                    tls_host = host.to_string();
                }
            }
        }

        if tls_host.len() > 0 {
            connman = connman.with_tls(cert, key, tls_host);
        }
    };

    // Add docker backend.
    let connman = connman.with_docker(arg.docker_host, arg.docker_port)?;
    let mut connman = connman.build()?;
    let sender = connman.sender();

    let receiver = connman.receiver().take().unwrap();

    tokio::spawn(connman.run(receiver));

    let addr = format!("{}:{}", arg.host, arg.port).parse()?;
    let db = String::from("database.sqlite");
    server::start_grpc(addr, sender, db).await
}

fn _setup_logger() -> Result<(), fern::InitError> {
    // Set up the logger for both stdout and a log file
    Dispatch::new()
        // Output to stdout
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{}][{}] {}",
                record.level(),
                record.target(),
                message
            ))
        })
        .level(LevelFilter::Info)
        .chain(std::io::stdout())
        // Output to a log file
        .chain(fern::log_file("output.log")?)
        .apply()?;
    Ok(())
}

fn load_certificates_from_pem(path: &Path) -> std::io::Result<Vec<Certificate>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let certs = rustls_pemfile::certs(&mut reader)?;
    Ok(certs.into_iter().map(Certificate).collect())
}

fn load_private_key_from_file(path: &Path) -> anyhow::Result<PrivateKey> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut keys = rustls_pemfile::pkcs8_private_keys(&mut reader)?;

    match keys.len() {
        0 => Err(anyhow!("No PKCS8-encoded private key found in {:?}", path)),
        1 => Ok(PrivateKey(keys.remove(0))),
        _ => Err(anyhow!(
            "More than one PKCS8-encoded private key found in {:?}",
            path
        )),
    }
}
