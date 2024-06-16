use std::{
    fs::File,
    io::BufReader,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::anyhow;
use argh::FromArgs;

mod connman;
mod docker;
mod proxy;
mod server;

use connman::{ConnmanBuilder, TcpPorxy, TlsProxy};
use docker::Env;

use fern::Dispatch;
use log::{info, LevelFilter};

use socket2::Socket;
use tokio::sync::oneshot;
use tokio_rustls::rustls::{Certificate, PrivateKey};

use docker::ImageOption;
use tonic::transport::Server;
use webpki::EndEntityCert;

#[derive(FromArgs, PartialEq, Debug)]
/// Top-level command.
struct TopLevel {
    #[argh(subcommand)]
    nested: Args,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand)]
enum Args {
    GrpcArg(GrpcArg),
    ConnManArg(ConnManArg),
}

const DEFAULT_LISTEN_PORT: u16 = 4242;

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand, name = "grpc")]
/// Starts a gRPC server for configuring the service.
struct GrpcArg {
    /// host address for starting gRPC server
    #[argh(option, short = 'h', default = "String::from(\"0.0.0.0\")")]
    host: String,

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
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand, name = "cli")]
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

    /// host address for deployment
    #[argh(option, short = 'h')]
    host: String,

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

    let arg: TopLevel = argh::from_env();
    let res = match arg.nested {
        Args::GrpcArg(arg) => start_grpc(arg).await,
        Args::ConnManArg(arg) => start_cli(arg).await,
    };
    return res;
}

async fn start_grpc(arg: GrpcArg) -> anyhow::Result<()> {
    let connman = ConnmanBuilder::new();
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

async fn start_cli(arg: ConnManArg) -> anyhow::Result<()> {
    let mut tls_enabled = false;
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
        for c in cert.iter() {
            let cert_der = c.as_ref();
            let cert = EndEntityCert::try_from(cert_der)?;
            for dns_name in cert.dns_names()? {
                let name: &str = dns_name.into();
                info!("Got Certificate for DNS: {}", name);
            }
        }
        connman = connman.with_tls(cert, key);
        tls_enabled = true;
    };

    // Add docker backend.
    let connman = connman.with_docker(arg.docker_host, arg.docker_port)?;
    let mut connman = connman.build()?;
    let sender = connman.sender();
    let receiver = connman.receiver().take().unwrap();
    tokio::spawn(connman.run(receiver));

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
            host: arg.host,
            image: image_id,
            env,
        };
        connman::Msg::TlsProxy(tls_proxy, channel.0)
    } else {
        let tcp_proxy = TcpPorxy {
            host: arg.host,
            listen_port,
            image: image_id,
            env,
        };
        connman::Msg::TcpPorxy(tcp_proxy, channel.0)
    };
    sender.send(msg).await?;
    let url = channel.1.await??;
    info!("Url: {}:{}", url.host, url.port);

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
