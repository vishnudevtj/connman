use std::{
    collections::HashMap,
    fmt::{self, Display},
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
    oneshot,
};

const CONTAINER_NAME_PREFIX: &str = "connman-";

pub enum Msg {
    Register(ImageOption, oneshot::Sender<Result<ImageId, Error>>),
    Create(CreateOption, oneshot::Sender<Result<ContainerId, Error>>),
    Start(ContainerId),
    Stop(ContainerId),
    UpdateStatus(ContainerId, bool),
}

#[derive(Clone)]
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

#[derive(Clone)]
pub struct CreateOption {
    // Determines from which image the container is created
    pub image_id: ImageId,

    // Name of the container
    pub container_name: String,

    // Env variable mapping
    pub env: Option<Env>,

    // Port binding on host
    pub port: u16,
}

#[derive(Clone)]
pub struct Env {
    pub key: String,
    pub value: String,
}

impl Display for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("{}={}", self.key, self.value))
    }
}

// Represend an Id for a docker container
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct ContainerId(pub String);

// Represend an Id for a docker container
#[derive(Clone, Eq, PartialEq, Hash, Debug)]
pub struct ImageId(String);

impl ImageId {
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

#[derive(Debug)]
pub enum Error {
    Internal(bollard::errors::Error),
    Pull(String),
    ImageNotFound,
}

impl std::error::Error for Error {}

pub struct DockerMan {
    docker: Docker,
    // Host address of docker backend.
    host: String,
    // Tracks the running status of the container.
    status: HashMap<ContainerId, bool>,
    // Registry containing details of all the images
    // in this docker host
    registry: HashMap<ImageId, ImageOption>,

    conn: (Sender<Msg>, Receiver<Msg>),
}

impl DockerMan {
    pub fn new(host: String, port: u16) -> anyhow::Result<Self> {
        let addr = format!("http://{}:{}", host, port);
        let docker = bollard::Docker::connect_with_http(&addr, 30, API_DEFAULT_VERSION)?;
        let status = HashMap::new();
        let conn = mpsc::channel(10);
        let registry = HashMap::new();

        Ok(Self {
            host,
            docker,
            status,
            registry,
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
                    if let Some(image_option) = self.registry.get(&option.image_id) {
                        let image_option = image_option.clone();

                        let future = async move {
                            let result = DockerMan::check_and_create_container(
                                docker,
                                option,
                                &image_option,
                                sender,
                            )
                            .await;
                            let _ = response.send(result);
                        };
                        tokio::spawn(future);
                    } else {
                        let _ = response.send(Err(Error::ImageNotFound));
                    };
                }

                Msg::Register(option, response) => {
                    let image_id = ImageId::from(&option);
                    self.registry.insert(image_id, option.clone());

                    let docker = self.docker.clone();
                    let fut = async move {
                        let result = DockerMan::check_and_pull_image(&docker, &option).await;
                        let _ = response.send(result);
                    };

                    tokio::spawn(fut);
                }
                Msg::Start(id) => {
                    let sender = self.sender();
                    let docker = self.docker.clone();
                    let _ = self
                        .status
                        .get(&id)
                        .map(|status| {
                            // Only try to start the contaienr if it's in stoped state.
                            if !status {
                                tokio::spawn(DockerMan::start_container(
                                    docker,
                                    id.clone(),
                                    sender,
                                ));
                            }
                        })
                        .ok_or_else(|| {
                            warn!("Got invalid ContainerId: {}", id.0);
                        });
                }
                Msg::Stop(id) => {
                    let sender = self.sender();
                    let docker = self.docker.clone();
                    let _ = self
                        .status
                        .get(&id)
                        .map(|status| {
                            // Only try to stop the container if it's in a running state.
                            if *status {
                                tokio::spawn(DockerMan::stop_container(docker, id.clone(), sender));
                            }
                        })
                        .ok_or_else(|| {
                            warn!("Got invalid ContainerId: {}", id.0);
                        });
                }

                Msg::UpdateStatus(id, status) => {
                    self.status
                        .entry(id)
                        .and_modify(|s| *s = status)
                        .or_insert(status);
                }
            }
        }
    }

    async fn start_container(docker: Docker, id: ContainerId, sender: Sender<Msg>) {
        info!("Starting container: {}", id.0);

        if docker
            .start_container(&id.0, None::<StartContainerOptions<String>>)
            .await
            .map_err(|err| error!("Unable to start container: {} : {}", id.0, err))
            .is_ok()
        {
            // Update the status of container as running.
            let _ = sender
                .send(Msg::UpdateStatus(id, true))
                .await
                .map_err(|err| {
                    error!(
                        "Unable to update the status of container while starting container :{err}"
                    )
                });
        }
    }

    async fn stop_container(docker: Docker, id: ContainerId, sender: Sender<Msg>) {
        info!("Stoping container: {}", id.0);

        if docker
            .stop_container(&id.0, None::<StopContainerOptions>)
            .await
            .map_err(|err| error!("Unable to stop container: {} : {}", id.0, err))
            .is_ok()
        {
            // Update the status of container as stopped.
            let _ = sender
                .send(Msg::UpdateStatus(id, false))
                .await
                .map_err(|err| {
                    error!(
                        "Unable to update the status of container while stoping container: {err}"
                    )
                });
        }
    }

    async fn check_and_create_container(
        docker: Docker,
        options: CreateOption,
        image_option: &ImageOption,
        sender: Sender<Msg>,
    ) -> Result<ContainerId, Error> {
        if let Some(id) = DockerMan::check_container(&docker, &options, image_option).await? {
            info!(
                "Not creating container<{}> as it already exists",
                options.container_name
            );

            // Set the running status of container as false.
            let _ = sender
                .send(Msg::UpdateStatus(id.clone(), false))
                .await
                .map_err(|err| error!("Unable to update status while creating container: {err}"));
            Ok(id)
        } else {
            DockerMan::create_container(&docker, options, image_option, sender).await
        }
    }

    async fn create_container(
        docker: &Docker,
        options: CreateOption,
        image_option: &ImageOption,
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

        let env = options.env.map(|x| vec![x.to_string()]);
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
            "Create Container from image : {} : {}",
            options.container_name, response.id
        );

        let id = ContainerId(response.id);

        // Set the running status of container as false.
        let _ = sender
            .send(Msg::UpdateStatus(id.clone(), false))
            .await
            .map_err(|err| {
                error!("Unable to update status of container when creating container: {err}")
            });

        Ok(id)
    }

    async fn check_container(
        docker: &Docker,
        options: &CreateOption,
        image_option: &ImageOption,
    ) -> Result<Option<ContainerId>, Error> {
        let l_options = ListContainersOptions::<String> {
            all: true,
            ..Default::default()
        };

        let summary = docker.list_containers(Some(l_options)).await?;
        for s in summary {
            let id = s.id;
            if s.image.map(|image| image == image_option.name).is_none() {
                continue;
            }
            if let Some(true) = s
                .names
                .map(|names| names.iter().any(|n| n.contains(&options.container_name)))
            {
                return Ok(id.map(ContainerId));
            }
        }
        Ok(None)
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

impl From<&ImageOption> for ImageId {
    fn from(value: &ImageOption) -> Self {
        let hash_key = Key(HASH_KEY);
        let mut hasher = HighwayHasher::new(hash_key);
        hasher.append(value.name.as_bytes());
        hasher.append(value.tag.as_bytes());
        hasher.append(&value.service_port.to_le_bytes());
        ImageId(hasher.finalize64().to_string())
    }
}
