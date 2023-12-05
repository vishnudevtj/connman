use std::{
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
};

use anyhow::anyhow;
use argh::FromArgs;

mod connman;
mod docker;
mod proxy;

use connman::{ConnmanBuilder, TcpPorxy, TlsProxy};
use docker::Env;

use fern::Dispatch;
use log::{info, LevelFilter};

use tokio::sync::oneshot;
use tokio_rustls::rustls::{Certificate, PrivateKey};

use docker::ImageOption;

const DEFAULT_LISTEN_PORT: u16 = 4242;

#[derive(FromArgs)]
/// Auto start docker container on TCP request.
struct ConnManArg {
    /// address of docker HTTP API server   
    #[argh(option, short = 'd')]
    docker_host: String,

    /// port on which docker HTTP API server
    /// listens
    #[argh(option, short = 'q')]
    docker_port: u16,

    /// name of the Docker image
    #[argh(option, short = 'i')]
    image: String,

    /// port exposed by the image
    #[argh(option, short = 'p')]
    service_port: u16,

    /// cert file
    #[argh(option, short = 'c')]
    cert: Option<PathBuf>,

    /// key file
    #[argh(option)]
    key: Option<PathBuf>,

    /// always pull image
    #[argh(option, short = 'b')]
    pull: Option<bool>,

    /// on which port to listen for incomming
    /// connection
    #[argh(option, short = 'l')]
    listen_port: Option<u16>,

    /// name of the environment variable on which the challenge
    /// expect a flag
    #[argh(option, short = 'k')]
    env_key: Option<String>,

    /// flag value which should be passed as the value for the
    /// environment variable epecified by the --env_kay option
    #[argh(option, short = 'v')]
    env_value: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    setup_logger()?;

    let mut tls_enabled = false;
    let arg: ConnManArg = argh::from_env();
    let mut listen_port = arg.listen_port.unwrap_or(DEFAULT_LISTEN_PORT);

    let env = if let (Some(key), Some(value)) = (arg.env_key, arg.env_value) {
        Some(Env { key, value })
    } else {
        None
    };

    let mut connman = ConnmanBuilder::new();
    let _msg = if let (Some(c), Some(k)) = (arg.cert, arg.key) {
        info!("Loading Certificate and Private Key");
        info!("Overriding Listen port to : 443");
        listen_port = 443;
        let cert = load_certificates_from_pem(&c)?;
        let key = load_private_key_from_file(&k)?;
        connman = connman.with_tls(cert, key);
        tls_enabled = true;
    };

    // Add docker backend.
    let connman = connman.with_docker(arg.docker_host, arg.docker_port)?;
    let connman = connman.build()?;
    let sender = connman.sender();
    tokio::spawn(connman.run());

    info!("Starting connman Server 0.3");

    let image_option = ImageOption {
        always_pull: arg.pull.unwrap_or(false),
        service_port: arg.service_port,
        name: arg.image.clone(),
        tag: String::from("latest"),
        credentials: None,
    };

    let channel = oneshot::channel();
    let msg = connman::Msg::RegisterImage(image_option, channel.0);
    sender.send(msg).await?;
    let image_id = channel.1.await??;

    let channel = oneshot::channel();
    let msg = if tls_enabled {
        let tls_proxy = TlsProxy {
            host: String::from(""),
            image: image_id,
            env,
        };
        connman::Msg::TlsProxy(tls_proxy, channel.0)
    } else {
        let tcp_proxy = TcpPorxy {
            listen_port,
            image: image_id,
            env,
        };
        connman::Msg::TcpPorxy(tcp_proxy, channel.0)
    };
    sender.send(msg).await?;
    let url = channel.1.await??;
    info!("Url: {}", url);

    // Wait indefinetly
    let channel = oneshot::channel::<bool>();
    let _ = channel.1.await;
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

fn setup_logger() -> Result<(), fern::InitError> {
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
