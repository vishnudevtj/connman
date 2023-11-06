use std::{collections::HashMap, net::Ipv4Addr};

use anyhow::Context;
use bollard::{
    container::{
        self, CreateContainerOptions, ListContainersOptions, StartContainerOptions,
        StopContainerOptions,
    },
    errors::Error,
    service::{HostConfig, PortBinding},
    Docker,
};
use log::{error, info};
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    oneshot,
};

const CONTAINER_NAME_PREFIX: &str = "connman-";

pub enum Msg {
    Create(CreateOption, oneshot::Sender<Result<Id, Error>>),
    Start(Id),
    Stop(Id),
}

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
pub struct Id(String);

pub struct DockerMan {
    docker: Docker,

    // Tracks the status of the container.
    status: HashMap<Id, bool>,

    conn: (Sender<Msg>, Receiver<Msg>),
}

impl DockerMan {
    pub fn new() -> anyhow::Result<Self> {
        let docker = Docker::connect_with_local_defaults()
            .context("Unable to connect to local docker socket")?;
        let status = HashMap::new();
        let conn = mpsc::channel(10);

        Ok(Self {
            docker,
            status,
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
                    let result = if let Some(id) = self.check_container(&option).await {
                        info!(
                            "Not creating container<{}> as it already exists",
                            option.container_name
                        );

                        // Set the running status of container as false.
                        self.status.insert(id.clone(), false);

                        Ok(id)
                    } else {
                        self.create_container(option).await
                    };
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

    async fn start_container(&mut self, id: Id) {
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

    async fn stop_container(&mut self, id: Id) {
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

    async fn create_container(&mut self, options: CreateOption) -> Result<Id, Error> {
        let service_port = format!("{}/tcp", options.service_port);
        let binding = vec![PortBinding {
            host_ip: Some(Ipv4Addr::LOCALHOST.to_string()),
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
            options.image_name, response.id
        );

        let id = Id(response.id);

        // Set the running status of container as false.
        self.status.insert(id.clone(), false);

        Ok(id)
    }

    async fn check_container(&mut self, options: &CreateOption) -> Option<Id> {
        let l_options = ListContainersOptions::<String> {
            all: true,
            ..Default::default()
        };

        let response = self.docker.list_containers(Some(l_options)).await;
        match response {
            Ok(summary) => {
                for s in summary {
                    let id = s.id;
                    if s.image
                        .and_then(|image| Some(image == options.image_name))
                        .is_none()
                    {
                        continue;
                    }
                    if let Some(true) = s.names.and_then(|names| {
                        Some(names.iter().any(|n| n.contains(&options.container_name)))
                    }) {
                        return id.map(|x| Id(x));
                    }
                }
            }
            Err(err) => {
                error!("Unable to check container: {}", err);
            }
        }
        None
    }
}
