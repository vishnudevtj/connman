use std::{
    collections::HashMap,
    fmt::{self, Display},
    hash::Hash,
    hash::Hasher,
    sync::Arc,
};

pub const HASH_KEY: [u64; 4] = [0xdeadbeef, 0xcafebabe, 0x4242, 0x6969];

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
use highway::HighwayHash;
use highway::{HighwayHasher, Key};
use log::{error, info, warn};
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    oneshot, Mutex,
};
use tokio_util::sync::CancellationToken;

use crate::{
    define_registry,
    id::{Registry, TypedId},
};

pub const CONTAINER_NAME_PREFIX: &str = "connman-";

pub enum Msg {
    Pull(ImageId, oneshot::Sender<Result<ImageId, Error>>),
    Create(ContainerOption, oneshot::Sender<Result<ContainerId, Error>>),
    Start(ContainerId),
    Stop(ContainerId),
    UpdateStatus(ContainerId, ContainerStatus),
}

#[derive(Debug)]
pub enum Error {
    Internal(bollard::errors::Error),
    Pull(String),
    ImageNotFound,
}

impl std::error::Error for Error {}

#[derive(Clone, Debug)]
pub struct ImageOption {
    // Pull the image even if it exists locally when the
    // image is registered
    pub always_pull: bool,

    // Name of the image to pull
    pub name: String,

    // Tag of the image to pull
    pub tag: String,

    // Port Exposed by the container
    pub service_port: u16,

    // Docker Registry Credentials
    pub credentials: Option<DockerCredentials>,
}

#[derive(Clone, Hash, PartialEq, Eq, Debug)]
pub struct ContainerOption {
    // Determines from which image the container is created
    pub image_id: ImageId,

    // Name of the container
    pub container_name: String,

    // Env variable mapping
    pub env: Option<Env>,

    // Port binding on docker host
    pub port: u16,
}

pub type ContainerId = TypedId<ContainerOption>;
pub type ImageId = TypedId<ImageOption>;

// ImageRegistry contains details of all images registered globally.
define_registry!(ImageRegistry, ImageOption);
// ContainerRegistry Contains details about all the Containers registered
// in a docker backend.
define_registry!(ContainerRegistry, ContainerOption);

#[derive(Clone, Hash, PartialEq, Eq, Debug)]
pub struct Env {
    pub key: String,
    pub value: String,
}

#[derive(PartialEq)]
enum ContainerState {
    Stopped,
    Running,
}
struct ContainerStatus {
    // TODO: Collect other metrics and health of the container.
    state: ContainerState,
}

// Manages all the containers associated with a docker backend.
pub struct DockerMan {
    // Bollard docker instance
    docker: Docker,
    // Host address of docker backend.
    host: String,
    // Tracks the running status of the container.
    status: HashMap<ContainerId, ContainerStatus>,
    // Registry containing details of all the images
    image_registry: ImageRegistry,
    // Registry containing details of all container registered in this
    // docker backend.
    container_registry: ContainerRegistry,
    // Tracks the status of a container in this docker backend.
    container_status: HashMap<ContainerId, ContainerStatus>,

    conn: (Sender<Msg>, Receiver<Msg>),
}

impl DockerMan {
    pub fn new(host: String, port: u16, registry: ImageRegistry) -> anyhow::Result<Self> {
        let addr = format!("http://{}:{}", host, port);
        let docker = bollard::Docker::connect_with_http(&addr, 30, API_DEFAULT_VERSION)?;
        let status = HashMap::new();
        let conn = mpsc::channel(10);
        let container_registry = ContainerRegistry::new();
        let container_status = HashMap::new();

        Ok(Self {
            host,
            docker,
            status,
            image_registry: registry,
            container_registry,
            container_status,
            conn,
        })
    }

    pub fn host(&self) -> String {
        self.host.clone()
    }

    pub fn sender(&self) -> Sender<Msg> {
        self.conn.0.clone()
    }

    pub async fn run(mut self) {
        while let Some(msg) = self.conn.1.recv().await {
            match msg {
                Msg::Create(option, response) => {
                    let sender = self.sender();
                    let docker = self.docker.clone();
                    let image_registry = self.image_registry.clone();
                    let container_registry = self.container_registry.clone();

                    let future = async move {
                        let result = DockerMan::check_and_create_container(
                            docker,
                            option,
                            image_registry,
                            container_registry,
                            sender,
                        )
                        .await;
                        let _ = response.send(result);
                    };
                    tokio::spawn(future);
                }

                Msg::Pull(image_id, response) => {
                    if let Some(image_option) = self.image_registry.get(&image_id).await {
                        let docker = self.docker.clone();

                        let fut = async move {
                            let result =
                                DockerMan::check_and_pull_image(&docker, &image_option).await;
                            let _ = response.send(result);
                        };

                        tokio::spawn(fut);
                    } else {
                        let result = Err(Error::ImageNotFound);
                        response.send(result);
                    }
                }
                Msg::Start(id) => {
                    let sender = self.sender();
                    let docker = self.docker.clone();
                    let container_registry = self.container_registry.clone();

                    let _ = self
                        .status
                        .get(&id)
                        .map(|status| {
                            // Only try to start the contaienr if it's in stoped state.
                            if status.state == ContainerState::Stopped {
                                tokio::spawn(DockerMan::start_container(
                                    docker,
                                    id.clone(),
                                    container_registry,
                                    sender,
                                ));
                            }
                        })
                        .ok_or_else(|| {
                            warn!("Got invalid ContainerId: {}", id);
                        });
                }
                Msg::Stop(id) => {
                    let sender = self.sender();
                    let docker = self.docker.clone();
                    let container_registry = self.container_registry.clone();

                    let _ = self
                        .status
                        .get(&id)
                        .map(|status| {
                            // Only try to stop the container if it's in a running state.
                            if status.state == ContainerState::Running {
                                tokio::spawn(DockerMan::stop_container(
                                    docker,
                                    id.clone(),
                                    container_registry,
                                    sender,
                                ));
                            }
                        })
                        .ok_or_else(|| {
                            warn!("Got invalid ContainerId: {}", id);
                        });
                }

                Msg::UpdateStatus(id, status) => {
                    self.status.insert(id, status);
                }
            }
        }
    }

    async fn start_container(
        docker: Docker,
        id: ContainerId,
        container_registry: ContainerRegistry,
        sender: Sender<Msg>,
    ) {
        if let Some(container_option) = container_registry.get(&id).await {
            let name = format!(
                "{}{}",
                CONTAINER_NAME_PREFIX, container_option.container_name
            );
            info!("Starting container: {}", &name);

            if docker
                .start_container(&name, None::<StartContainerOptions<String>>)
                .await
                .map_err(|err| error!("Unable to start container: {} : {}", id, err))
                .is_ok()
            {
                let container_status = ContainerStatus {
                    state: ContainerState::Running,
                };
                // Update the status of container as running.
                DockerMan::update_status(sender, id, container_status).await;
            }
        }
    }

    async fn stop_container(
        docker: Docker,
        id: ContainerId,
        container_registry: ContainerRegistry,
        sender: Sender<Msg>,
    ) {
        if let Some(container_option) = container_registry.get(&id).await {
            let name = format!(
                "{}{}",
                CONTAINER_NAME_PREFIX, container_option.container_name
            );

            info!("Stoping container: {}", &name);
            if docker
                .stop_container(&name, None::<StopContainerOptions>)
                .await
                .map_err(|err| error!("Unable to stop container: {} : {}", id, err))
                .is_ok()
            {
                let container_status = ContainerStatus {
                    state: ContainerState::Stopped,
                };

                // Update the status of container as stopped.
                DockerMan::update_status(sender, id, container_status).await;
            }
        }
    }

    async fn check_and_create_container(
        docker: Docker,
        options: ContainerOption,
        image_registry: ImageRegistry,
        container_registry: ContainerRegistry,
        sender: Sender<Msg>,
    ) -> Result<ContainerId, Error> {
        let image_option = image_registry
            .get(&options.image_id)
            .await
            .ok_or(Error::ImageNotFound)?;
        if DockerMan::check_container(&docker, &options, &image_option).await? {
            info!(
                "Not creating container<{}> as it already exists",
                options.container_name
            );
            // Update the container registry.
            let container_id = container_registry.register(options.clone()).await;

            // Set the status of container as false.
            let container_status = ContainerStatus {
                state: ContainerState::Stopped,
            };

            DockerMan::update_status(sender, container_id.clone(), container_status).await;
            Ok(container_id)
        } else {
            DockerMan::create_container(&docker, options, &image_option, container_registry, sender)
                .await
        }
    }

    async fn create_container(
        docker: &Docker,
        options: ContainerOption,
        image_option: &ImageOption,
        container_registry: ContainerRegistry,
        sender: Sender<Msg>,
    ) -> Result<ContainerId, Error> {
        info!("Creating Container from image: {}", &image_option.name);

        // Check if the image exists, if not pull the image.
        DockerMan::check_and_pull_image(docker, image_option).await?;

        let service_port = format!("{}/tcp", image_option.service_port);
        let binding = vec![PortBinding {
            host_ip: Some(String::from("0.0.0.0")),
            host_port: Some(options.port.to_string()),
        }];

        let host_config = Some(HostConfig {
            port_bindings: Some(HashMap::from([(service_port.clone(), Some(binding))])),
            ..Default::default()
        });

        let env = options.env.clone().map(|x| vec![x.to_string()]);
        let config = container::Config {
            image: Some(image_option.name.clone()),
            env,
            host_config,
            exposed_ports: Some(HashMap::from([(service_port, HashMap::new())])),
            ..Default::default()
        };

        let container_options = CreateContainerOptions {
            name: format!("{}{}", CONTAINER_NAME_PREFIX, options.container_name),
            platform: None,
        };

        let response = docker
            .create_container(Some(container_options), config)
            .await?;

        info!(
            "Created Container from image : {} : {}",
            options.container_name, response.id
        );

        // Update the container registry.
        let container_id = container_registry.register(options).await;

        // Set the status of container as false.
        let container_status = ContainerStatus {
            state: ContainerState::Stopped,
        };

        // Set the running status of container as false.
        DockerMan::update_status(sender, container_id.clone(), container_status).await;

        Ok(container_id)
    }

    async fn check_container(
        docker: &Docker,
        options: &ContainerOption,
        image_option: &ImageOption,
    ) -> Result<bool, Error> {
        let l_options = ListContainersOptions::<String> {
            all: true,
            ..Default::default()
        };

        let summary = docker.list_containers(Some(l_options)).await?;
        for s in summary {
            if s.image.map(|image| image == image_option.name).is_none() {
                continue;
            }
            if let Some(true) = s
                .names
                .map(|names| names.iter().any(|n| n.contains(&options.container_name)))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn check_image(docker: &Docker, options: &ImageOption) -> Result<Option<ImageId>, Error> {
        let c_option = ListImagesOptions::<String> {
            all: true,
            ..Default::default()
        };

        let summary = docker.list_images(Some(c_option)).await?;
        for summary in summary {
            let result = summary
                .repo_tags
                .iter()
                .any(|name| name.contains(&options.name));
            if result {
                return Ok(Some(ImageId::from(options)));
            }
        }

        Ok(None)
    }

    async fn pull_image(docker: &Docker, options: &ImageOption) -> Result<ImageId, Error> {
        use futures_util::stream::StreamExt;
        let c_option = CreateImageOptions {
            from_image: options.name.clone(),
            tag: options.tag.clone(),
            ..Default::default()
        };

        let mut response = docker.create_image(Some(c_option), None, options.credentials.clone());

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

        let id = DockerMan::check_image(docker, options).await?;
        id.ok_or_else(|| Error::Pull(String::from("Unable to find Image Locally")))
    }

    async fn check_and_pull_image(docker: &Docker, option: &ImageOption) -> Result<ImageId, Error> {
        if option.always_pull {
            return DockerMan::pull_image(docker, option).await;
        }

        if let Some(id) = DockerMan::check_image(docker, option).await? {
            info!("Not pulling Image <{}> as it already exists", option.name);
            Ok(id)
        } else {
            DockerMan::pull_image(docker, option).await
        }
    }
    async fn update_status(sender: Sender<Msg>, id: ContainerId, status: ContainerStatus) {
        let _ = sender
            .send(Msg::UpdateStatus(id, status))
            .await
            .map_err(|err| {
                error!("Unable to update the status of container while stoping container: {err}")
            });
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

impl Display for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("{}={}", self.key, self.value))
    }
}

impl Hash for ImageOption {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.always_pull.hash(state);
        self.name.hash(state);
        self.tag.hash(state);
        self.service_port.hash(state);
        // Note: We're intentionally not hashing the credentials field
    }
}

impl PartialEq for ImageOption {
    fn eq(&self, other: &Self) -> bool {
        self.always_pull == other.always_pull
            && self.name == other.name
            && self.tag == other.tag
            && self.service_port == other.service_port
        // Note: We're intentionally not comparing the credentials field
    }
}

impl Eq for ImageOption {}
