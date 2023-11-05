use std::{collections::HashMap, net::Ipv4Addr};

use anyhow::Context;
use bollard::{
    container::{self, CreateContainerOptions, StartContainerOptions, StopContainerOptions},
    errors::Error,
    service::{HostConfig, PortBinding},
    Docker,
};
use log::{error, info};
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    oneshot,
};

pub enum Msg {
    Create(CreateOption, oneshot::Sender<Result<Id, Error>>),
    Start(Id),
    Stop(Id),
}

pub struct CreateOption {
    // Container Image to start.
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
                    let result = self.create_container(option).await;
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

        // TODO: Assign random name for created container.

        let response = self
            .docker
            .create_container(None::<CreateContainerOptions<String>>, config)
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
}
