use argh::FromArgs;
use env_logger::Builder;
use log::{error, info, LevelFilter};

mod docker;
mod proxy;

const PROXY_PORT: u16 = 4243;
const LISTEN_PORT: u16 = 4242;

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

    /// always pull image
    #[argh(option, short = 'b')]
    pull: Option<bool>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut builder = Builder::from_default_env();
    builder.filter_level(LevelFilter::Info);
    builder.init();

    let connman: ConnMan = argh::from_env();

    info!("Starting connman Server 0.1");

    let docker_socket_addr = format!("{}:{}", &connman.docker_host, connman.docker_port);
    let docker_man = docker::DockerMan::new(docker_socket_addr)?;

    let container_name = String::from("clatter-calculate");
    let proxy = proxy::Proxy::new(
        LISTEN_PORT,
        connman.image.clone(),
        connman.service_port,
        PROXY_PORT,
        connman.docker_host,
        container_name,
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
