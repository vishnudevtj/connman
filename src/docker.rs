use std::{collections::HashMap, fmt, net::Ipv4Addr};

use anyhow::Context;
use bollard::{
    auth::DockerCredentials,
    container::{
        self, CreateContainerOptions, ListContainersOptions, StartContainerOptions,
        StopContainerOptions,
    },
    image::{CreateImageOptions, ListImagesOptions},
    service::{HostConfig, PortBinding},
    Docker, API_DEFAULT_VERSION,
};
use log::{error, info};
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    oneshot,
};

const CONTAINER_NAME_PREFIX: &str = "connman-";

pub enum Msg {
    Register(ImageOption, oneshot::Sender<Result<ImageId, Error>>),
    Create(CreateOption, oneshot::Sender<Result<ContainerId, Error>>),
    Start(ContainerId),
    Stop(ContainerId),
}

#[derive(Clone)]
pub struct ImageOption {
    // Pull the image even if it exists locally
    pub always_pull: bool,

    // Name of the image to pull
    pub name: String,

    // Tag of the image to pull
    pub tag: String,

    // Docker Registry Credentials
    pub credentials: Option<DockerCredentials>,
}

#[derive(Clone)]
pub struct CreateOption {
    // Name of the container
    pub container_name: String,

    // Container Image to start
    pub image_name: String,

    // Port Exposed by the container
    pub service_port: u16,

    // Port binding on host
    pub port: u16,
}

// Represend an Id for a docker container
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct ContainerId(String);

// Represend an Id for a docker container
#[derive(Clone, Eq, PartialEq, Hash, Debug)]
pub struct ImageId(String);

pub enum Error {
    Internal(bollard::errors::Error),
    Pull(String),
    ImageNotFound,
}
pub struct DockerMan {
    docker: Docker,

    // Tracks the status of the container.
    status: HashMap<ContainerId, bool>,

    // Tracks all the registers images
    registry: HashMap<String, ImageOption>,

    conn: (Sender<Msg>, Receiver<Msg>),
}

impl DockerMan {
    pub fn new(addr: String) -> anyhow::Result<Self> {
        let docker = Docker::connect_with_http(&addr, 30, API_DEFAULT_VERSION)?;
        // let docker = Docker::connect_with_local_defaults()
        //     .context("Unable to connect to local docker socket")?;
        let status = HashMap::new();
        let conn = mpsc::channel(10);
        let registry = HashMap::new();

        Ok(Self {
            docker,
            status,
            registry,
            conn,
        })
    }

    pub fn sender(&self) -> Sender<Msg> {
        self.conn.0.clone()
    }

    pub async fn run(mut self) {
        while let Some(msg) = self.conn.1.recv().await {
            match msg {
                Msg::Create(option, response) => {
                    let result = async {
                        if let Some(id) = self.check_container(&option).await? {
                            info!(
                                "Not creating container<{}> as it already exists",
                                option.container_name
                            );

                            // Set the running status of container as false.
                            self.status.insert(id.clone(), false);
                            Ok(id)
                        } else {
                            self.create_container(option).await
                        }
                    }
                    .await;
                    let _ = response.send(result);
                }

                Msg::Register(option, response) => {
                    self.registry.insert(option.name.clone(), option.clone());
                    let result = self.check_and_pull_image(&option).await;
                    let _ = response.send(result);
                }
                Msg::Start(id) => {
                    self.start_container(id).await;
                }
                Msg::Stop(id) => {
                    self.stop_container(id).await;
                }
            }
        }
    }

    async fn start_container(&mut self, id: ContainerId) {
        if let Some(status) = self.status.get_mut(&id) {
            if !*status {
                let _ = self
                    .docker
                    .start_container(&id.0, None::<StartContainerOptions<String>>)
                    .await
                    .map_err(|err| error!("Unable to start container: {} : {}", id.0, err));

                info!("Starting container: {}", id.0);

                // Update the status of container as running.
                *status = true;
            }
        } else {
            error!(
                "Unable to start container: {} : Id not found in Internal Map",
                id.0
            );
        }
    }

    async fn stop_container(&mut self, id: ContainerId) {
        if let Some(status) = self.status.get_mut(&id) {
            if *status {
                let _ = self
                    .docker
                    .stop_container(&id.0, None::<StopContainerOptions>)
                    .await
                    .map_err(|err| error!("Unable to stop container: {} : {}", id.0, err));

                info!("Stoping container: {}", id.0);

                // Update the status of container as stopped.
                *status = false;
            }
        } else {
            error!(
                "Unable to stop container: {} : Id not found in Internal Map",
                id.0
            );
        }
    }

    async fn create_container(&mut self, options: CreateOption) -> Result<ContainerId, Error> {
        let image_option = self
            .registry
            .get(&options.image_name)
            .ok_or(Error::ImageNotFound)?
            .clone();
        self.check_and_pull_image(&image_option).await?;

        let service_port = format!("{}/tcp", options.service_port);
        let binding = vec![PortBinding {
            host_ip: Some(String::from("0.0.0.0")),
            host_port: Some(options.port.to_string()),
        }];

        let host_config = Some(HostConfig {
            port_bindings: Some(HashMap::from([(service_port.clone(), Some(binding))])),
            ..Default::default()
        });

        let config = container::Config {
            image: Some(options.image_name.clone()),
            host_config,
            exposed_ports: Some(HashMap::from([(service_port, HashMap::new())])),
            ..Default::default()
        };

        let container_options = CreateContainerOptions {
            name: format!("{}{}", CONTAINER_NAME_PREFIX, options.container_name),
            platform: None,
        };

        let response = self
            .docker
            .create_container(Some(container_options), config)
            .await?;

        info!(
            "Create Container from image : {} : {}",
            options.container_name, response.id
        );

        let id = ContainerId(response.id);

        // Set the running status of container as false.
        self.status.insert(id.clone(), false);

        Ok(id)
    }

    async fn check_container(
        &mut self,
        options: &CreateOption,
    ) -> Result<Option<ContainerId>, Error> {
        let l_options = ListContainersOptions::<String> {
            all: true,
            ..Default::default()
        };

        let summary = self.docker.list_containers(Some(l_options)).await?;
        for s in summary {
            let id = s.id;
            if s.image
                .and_then(|image| Some(image == options.image_name))
                .is_none()
            {
                continue;
            }
            if let Some(true) = s
                .names
                .and_then(|names| Some(names.iter().any(|n| n.contains(&options.container_name))))
            {
                return Ok(id.map(|x| ContainerId(x)));
            }
        }
        Ok(None)
    }

    async fn check_image(&mut self, options: &ImageOption) -> Result<Option<ImageId>, Error> {
        let c_option = ListImagesOptions::<String> {
            all: true,
            ..Default::default()
        };

        let summary = self.docker.list_images(Some(c_option)).await?;
        for summary in summary {
            let result = summary
                .repo_tags
                .iter()
                .any(|name| name.contains(&options.name));
            if result {
                return Ok(Some(ImageId(summary.id)));
            }
        }

        Ok(None)
    }

    async fn pull_image(&mut self, options: &ImageOption) -> Result<ImageId, Error> {
        use futures_util::stream::StreamExt;
        let c_option = CreateImageOptions {
            from_image: options.name.clone(),
            tag: options.tag.clone(),
            ..Default::default()
        };

        let mut response =
            self.docker
                .create_image(Some(c_option), None, options.credentials.clone());

        while let Some(result) = response.next().await {
            let info = result?;
            if let (Some(status), Some(progress)) = (info.status, info.progress) {
                info!("Pulling image<{}> :{}\t{}", &options.name, status, progress);
            }
            if let (Some(error), Some(error_detail)) = (info.error, info.error_detail) {
                error!(
                    "Error Pulling image<{}> : {:?}",
                    &options.name, error_detail
                );
                Err(Error::Pull(error))?
            }
        }

        let id = self.check_image(options).await?;
        id.ok_or_else(|| Error::Pull(String::from("Unable to find Image Locally")))
    }

    async fn check_and_pull_image(&mut self, option: &ImageOption) -> Result<ImageId, Error> {
        if option.always_pull {
            return self.pull_image(&option).await;
        }

        if let Some(id) = self.check_image(&option).await? {
            info!("Not pulling Image <{}> as it already exists", option.name);
            Ok(id)
        } else {
            self.pull_image(&option).await
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Internal(err) => {
                write!(f, "Internal Error: {}", err)
            }
            Error::Pull(err) => {
                write!(f, "Pull Error: {}", err)
            }
            Error::ImageNotFound => {
                write!(f, "Image not found in the Internal Registry Map")
            }
        }
    }
}

impl From<bollard::errors::Error> for Error {
    fn from(value: bollard::errors::Error) -> Self {
        Error::Internal(value)
    }
}
