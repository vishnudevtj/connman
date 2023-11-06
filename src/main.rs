use env_logger::Builder;
use log::{error, info, LevelFilter};

mod docker;
mod proxy;

const IMAGE_NAME: &str = "aswinmguptha/flashy_machine";
const SERVICE_PORT: u16 = 5000;
const PROXY_PORT: u16 = 4243;
const LISTEN_PORT: u16 = 4242;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut builder = Builder::from_default_env();
    builder.filter_level(LevelFilter::Info);
    builder.init();

    info!("Starting connman Server");

    let docker_man = docker::DockerMan::new()?;

    let container_name = String::from("clatter-calculate");
    let proxy = proxy::Proxy::new(
        LISTEN_PORT,
        String::from(IMAGE_NAME),
        SERVICE_PORT,
        PROXY_PORT,
        container_name,
        docker_man.sender(),
    );

    let sender = docker_man.sender();
    let fut = async move {
        let option = docker::ImageOption {
            always_pull: true,
            name: String::from(IMAGE_NAME),
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
