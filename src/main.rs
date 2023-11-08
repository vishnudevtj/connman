use std::{
    fs::File,
    io::{BufReader},
    path::{Path, PathBuf},
};

use anyhow::anyhow;
use argh::FromArgs;
use env_logger::Builder;
use log::{error, info, LevelFilter};


use tokio_rustls::rustls::{Certificate, PrivateKey};

mod docker;
mod proxy;

const PROXY_PORT: u16 = 4243;
const DEFAULT_LISTEN_PORT: u16 = 4242;

#[derive(FromArgs)]
/// Auto start docker container on TCP request.
struct ConnMan {
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
    #[argh(option, short = 'k')]
    key: Option<PathBuf>,

    /// always pull image
    #[argh(option, short = 'b')]
    pull: Option<bool>,

    /// on which port to listen for incomming
    /// connection
    #[argh(option, short = 'l')]
    listen_port: Option<u16>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut builder = Builder::from_default_env();
    builder.filter_level(LevelFilter::Info);
    builder.init();

    let connman: ConnMan = argh::from_env();
    let mut cert = None;
    let mut key = None;
    let mut listen_port = connman.listen_port.unwrap_or(DEFAULT_LISTEN_PORT);

    if let (Some(c), Some(k)) = (connman.cert, connman.key) {
        info!("Loading Certificate and Private Key");
        listen_port = 443;
        cert = Some(load_certificates_from_pem(&c)?);
        key = Some(load_private_key_from_file(&k)?);
    }

    info!("Starting connman Server 0.1");

    let docker_socket_addr = format!("{}:{}", &connman.docker_host, connman.docker_port);
    let docker_man = docker::DockerMan::new(docker_socket_addr)?;

    let container_name = String::from("clatter-calculate");
    let proxy = proxy::Proxy::new(
        listen_port,
        connman.image.clone(),
        connman.service_port,
        PROXY_PORT,
        connman.docker_host,
        container_name,
        cert,
        key,
        docker_man.sender(),
    );

    let sender = docker_man.sender();
    let fut = async move {
        let option = docker::ImageOption {
            always_pull: connman.pull.unwrap_or(false),
            name: connman.image.clone(),
            tag: String::from("latest"),
            credentials: None,
        };

        let response = tokio::sync::oneshot::channel();
        let _ = sender
            .send(docker::Msg::Register(option.clone(), response.0))
            .await;
        if let Ok(id) = response
            .1
            .await
            .unwrap()
            .map_err(|err| error!("Unable to pull Image<{}> : {}", option.name, err))
        {
            info!("Pulled Container Image : {} : {:?}", option.name, id)
        }

        proxy.run().await;
    };

    tokio::spawn(fut);

    docker_man.run().await;
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
