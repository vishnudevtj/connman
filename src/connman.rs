use std::sync::Arc;

use crate::docker::Env;
use crate::docker::ImageOption;

use crate::docker::ContainerId;
use crate::docker::ContainerOption;
use crate::docker::ImageRegistry;
use crate::docker::{DockerMan, ImageId};

use crate::docker;
use crate::proxy::Proxy;
use crate::proxy::ProxyId;
use crate::proxy::ProxyRegistry;
use crate::proxy::TlsMsg;
use crate::proxy::{TcpListener, TlsListener};
use crate::tui;
use crate::tui::ProxyInfo;
use crate::tui::TuiSender;

use highway::HighwayHasher;
use highway::Key;
use log::error;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;

use tokio::sync::Mutex;
use tokio::sync::{
    mpsc::{Receiver, Sender},
    oneshot,
};
use tokio_rustls::rustls::{Certificate, PrivateKey};

pub const HASH_KEY: [u64; 4] = [0xdeadbeef, 0xcafebabe, 0x4242, 0x6969];

const TLS_PORT: u16 = 443;

pub struct TlsProxy {
    // SNI address for proxying.
    pub host: String,
    // Id of the image to deploy for proxying.
    pub image: Id,
    // Environment variables for the container.
    pub env: Option<Env>,
}

#[derive(Clone)]
pub struct TcpProxy {
    // host address
    pub host: String,
    // Port on proxy host to listen.
    pub listen_port: u16,
    // Id of the image to deploy for proxying.
    pub image_id: Id,
    // Environment variables for the container.
    pub env: Option<Env>,
}

// Represent the Url on which the proxy is active.
pub struct Url {
    // host address of the proxy
    pub host: String,
    // port on which we listen for connection for a
    // specific proxy
    pub port: u16,
}

#[derive(Clone, Copy)]
pub struct Id(pub u64);

// TODO: Improve Error return. Have a custom type for all defined errors
pub enum Msg {
    // Deploys a challenge and returns the URL to access it along with the Id - ProxyId.
    TlsProxy(TlsProxy, oneshot::Sender<Result<(Id, Url)>>),
    // Deploys a TCP challenge and retuns host:port along with the Id - ProxyId.
    TcpProxy(TcpProxy, oneshot::Sender<Result<(Id, Url)>>),
    // Shutdown a proxy associated with an id - ProxyId.
    StopProxy(Id, oneshot::Sender<Result<ProxyId>>),
    // Register a container image on all available docker backend and returns an Id - ImageId.
    RegisterImage(ImageOption, oneshot::Sender<Result<Id>>),
}

pub struct ConnmanBuilder {
    // Contains TLS certificate and private key for SNI based routing.
    tls: Option<(Vec<Certificate>, PrivateKey)>,
    // List of docker backend.
    docker: Vec<DockerMan>,
    image_registry: ImageRegistry,
}

impl ConnmanBuilder {
    pub fn new() -> Self {
        Self {
            tls: None,
            docker: Vec::new(),
            image_registry: ImageRegistry::new(),
        }
    }

    pub fn with_tls(mut self, cert: Vec<Certificate>, key: PrivateKey) -> Self {
        self.tls = Some((cert, key));
        self
    }

    pub fn with_docker(mut self, addr: String, port: u16) -> Result<Self> {
        let docker_man = DockerMan::new(addr, port, self.image_registry.clone())?;
        self.docker.push(docker_man);
        Ok(self)
    }

    pub fn build(self) -> anyhow::Result<Connman> {
        if self.docker.is_empty() {
            return Err(anyhow!("No Docker backend configured!"));
        }
        let proxy_registry = ProxyRegistry::new();

        let tls = self.tls.map(|x| {
            let listener = TlsListener::new(TLS_PORT, x.0, x.1, proxy_registry.clone());
            let map = listener.sender();
            let fut = async {
                listener
                    .run()
                    .await
                    .map_err(|err| error!("Unable to start TLSListener: {err}"))
            };
            tokio::spawn(fut);
            map
        });

        let docker = self
            .docker
            .into_iter()
            .map(|docker| {
                let sender = docker.sender();
                let host = docker.host();
                let port_range = PortRange::new(30_000, 60_000);
                let back_end = DockerBackEnd {
                    host,
                    sender,
                    port_range,
                };
                tokio::spawn(docker.run());
                back_end
            })
            .collect();

        let conn = tokio::sync::mpsc::channel(10);
        let tui = tui::TUI_SENDER.get().cloned();

        Ok(Connman {
            tls,
            docker,
            sender: conn.0,
            tui,
            image_register: self.image_registry,
            proxy_registry,
            receiver: Some(conn.1),
        })
    }
}

pub struct Connman {
    // Channel to communicate with TLSListener.
    tls: Option<Sender<TlsMsg>>,

    // List of all available docker backend.
    docker: Vec<DockerBackEnd>,

    // Registery containning all docker image details.
    image_register: ImageRegistry,

    // Registry containning all active proxy
    proxy_registry: ProxyRegistry,

    // Sender to communicate with TUI
    tui: Option<TuiSender>,

    // Communication channel for the structure
    sender: Sender<Msg>,
    receiver: Option<Receiver<Msg>>,
}

impl Connman {
    pub async fn run(self, mut receiver: Receiver<Msg>) {
        let connman = Arc::new(self);
        while let Some(msg) = receiver.recv().await {
            let connman = connman.clone();
            let future = async move {
                connman.handle_message(msg).await;
            };
            tokio::spawn(future);
        }
    }

    async fn handle_message(&self, msg: Msg) {
        match msg {
            Msg::RegisterImage(image_option, result) => {
                let docker = self.get_docker_man();
                let image_id = self.image_register.register(image_option).await;
                let r = docker.pull_image(image_id).await.map(|x| Id(x.value()));
                let _ = result.send(r);
            }
            Msg::TlsProxy(tls_option, result) => {
                let r = self
                    .handle_tls_proxy(tls_option)
                    .await
                    .map(|x| (Id(x.0.value()), x.1));
                let _ = result.send(r);
            }
            Msg::TcpProxy(tcp_option, result) => {
                let r = self
                    .handle_tcp_proxy(tcp_option)
                    .await
                    .map(|x| (Id(x.0.value()), x.1));
                let _ = result.send(r);
            }
            Msg::StopProxy(proxy_id, result) => {
                if let Some(proxy_id) = self.proxy_registry.is_valid(proxy_id.0).await {
                    let r = self.stop_proxy(proxy_id).await;
                    let _ = result.send(r);
                } else {
                    let r = Err(anyhow!("Invalid ProxyId"));
                    let _ = result.send(r);
                }
            }
        }
    }

    pub fn sender(&self) -> Sender<Msg> {
        self.sender.clone()
    }

    pub fn receiver(&mut self) -> Option<Receiver<Msg>> {
        self.receiver.take()
    }

    async fn stop_proxy(&self, proxy_id: ProxyId) -> Result<ProxyId> {
        if let Some(proxy) = self.proxy_registry.remove(&proxy_id).await {
            proxy.cleanup().await?;
            Ok(proxy_id)
        } else {
            Err(anyhow!("ProxyId not found"))
        }
    }

    async fn handle_tcp_proxy(&self, tcp_option: TcpProxy) -> Result<(ProxyId, Url)> {
        let docker = self.get_docker_man();
        let proxy_host = docker.host.clone();
        let image_id = self
            .image_register
            .is_valid(tcp_option.image_id.0)
            .await
            .ok_or(anyhow!("Invalid ImageId"))?;

        let image_option = self
            .image_register
            .get(&image_id)
            .await
            .ok_or(anyhow!("Invalid ImageId"))?;

        let docker_port = docker
            .port_range
            .aquire_port()
            .await
            .ok_or(anyhow!("No port left on docker host"))?;

        // Generate a unique name for Container
        // TODO: Use ContainerId value ??
        use highway::HighwayHash;
        let hash_key = Key(HASH_KEY);
        let mut hasher = HighwayHasher::new(hash_key);
        hasher.append(&tcp_option.image_id.0.to_le_bytes());
        hasher.append(&tcp_option.listen_port.to_le_bytes());
        if let Some(env) = tcp_option.env.as_ref() {
            hasher.append(env.key.as_bytes());
            hasher.append(env.value.as_bytes());
        };

        let id = hasher.finalize64();
        let container_name = id.to_string();

        let create_option = ContainerOption {
            image_id,
            container_name,
            env: tcp_option.env.clone(),
            port: docker_port,
        };

        let container_id = docker.create_container(create_option).await?;
        let proxy_port = docker_port;
        let proxy = Arc::new(Proxy::new(
            container_id,
            proxy_port,
            proxy_host.clone(),
            tcp_option.env.clone(),
            docker.sender(),
        ));

        let proxy_id = self.proxy_registry.register(proxy.clone()).await;
        let proxy_registry = self.proxy_registry.clone();

        let listener = TcpListener::new(tcp_option.listen_port, proxy.clone());
        let proxy_id_1 = proxy_id.clone();
        let fut = async move {
            let _ = listener
                .run()
                .await
                .map_err(|err| error!("TcpListener Returned with Error: {err}"));

            // Cleanup Proxy After the listener has stopped working. Remove the proxy from the registry
            // Also run cleanup routine for the proxy.
            proxy_registry.remove(&proxy_id_1).await;
            let _ = proxy.cleanup().await;
        };
        tokio::spawn(fut);

        let docker_image = format!("{}:{}", image_option.name, image_option.tag);
        let proxy_info = ProxyInfo {
            id: proxy_id.value(),
            host_port: tcp_option.listen_port,
            docker_image,
            docker_host: proxy_host,
            docker_port: proxy_port,
        };

        if let Some(sender) = &self.tui {
            let _ = sender
                .send(tui::Msg::Proxy(proxy_info))
                .map_err(|err| error!("Unable to send message to TUI: {}", err));
        }

        let url = Url {
            host: tcp_option.host.clone(),
            port: proxy_port,
        };
        Ok((proxy_id, url))
    }

    async fn handle_tls_proxy(&self, tls_option: TlsProxy) -> Result<(ProxyId, Url)> {
        let docker = self.get_docker_man();

        let port = docker
            .port_range
            .aquire_port()
            .await
            .ok_or(anyhow!("No port left on docker host"))?;

        match &self.tls {
            Some(sender) => {
                let docker = self.get_docker_man();

                let image_id = self
                    .image_register
                    .is_valid(tls_option.image.0)
                    .await
                    .ok_or(anyhow!("Invalid ImageId"))?;

                use highway::HighwayHash;
                let hash_key = Key(HASH_KEY);
                let mut hasher = HighwayHasher::new(hash_key);
                hasher.append(&tls_option.image.0.to_le_bytes());
                hasher.append(tls_option.host.as_bytes());
                if let Some(env) = tls_option.env.as_ref() {
                    hasher.append(env.key.as_bytes());
                    hasher.append(env.value.as_bytes());
                };
                let name = hasher.finalize64().to_string();

                let create_option = ContainerOption {
                    image_id,
                    container_name: name,
                    env: tls_option.env.clone(),
                    port,
                };

                let container_id = docker.create_container(create_option).await?;

                let proxy = Arc::new(Proxy::new(
                    container_id,
                    port,
                    docker.host.clone(),
                    tls_option.env.clone(),
                    docker.sender(),
                ));
                let proxy_id = self.proxy_registry.register(proxy.clone()).await;

                sender
                    .send(TlsMsg::Add(tls_option.host.clone(), proxy_id.clone()))
                    .await?;

                let url = Url {
                    host: tls_option.host.clone(),
                    port: TLS_PORT,
                };
                Ok((proxy_id, url))
            }
            None => Err(anyhow!("TLS Listener not configured")),
        }
    }

    fn get_docker_man(&self) -> &DockerBackEnd {
        let rand: usize = rand::random();
        &self.docker[rand % self.docker.len()]
    }
}

#[derive(Clone)]
pub struct PortRange {
    inner: Arc<Mutex<PortRangeInner>>,
}

pub struct PortRangeInner {
    start: u16,
    _end: u16,
    used: Vec<bool>,
}

impl PortRange {
    pub fn new(start: u16, end: u16) -> Self {
        assert!(end > start);
        let used = vec![false; (end - start) as usize];
        let inner = PortRangeInner {
            start,
            _end: end,
            used,
        };
        let inner = Arc::new(Mutex::new(inner));
        Self { inner }
    }

    pub async fn aquire_port(&self) -> Option<u16> {
        let mut inner = self.inner.lock().await;
        let start = inner.start;
        for (idx, used) in inner.used.iter_mut().enumerate() {
            if !(*used) {
                let port = start + idx as u16;
                *used = true;
                return Some(port);
            }
        }
        None
    }

    pub async fn _release_port(&self, port: u16) {
        let mut inner = self.inner.lock().await;
        if port > inner._end {
            error!("release_port({}) : Ivalid port given", port);
            return;
        }

        let id = port - inner.start;
        inner.used[id as usize] = false;
    }
}

// Structure which contains details about a specific docker backend.
#[derive(Clone)]
struct DockerBackEnd {
    host: String,
    sender: Sender<docker::Msg>,
    port_range: PortRange,
}

impl DockerBackEnd {
    async fn pull_image(&self, image_id: ImageId) -> Result<ImageId> {
        let response = oneshot::channel();
        self.sender
            .send(docker::Msg::Pull(image_id, response.0))
            .await
            .context("Unable to send docker::Msg::Resgister msg")?;
        Ok(response.1.await??)
    }

    fn sender(&self) -> Sender<docker::Msg> {
        self.sender.clone()
    }

    async fn create_container(&self, option: ContainerOption) -> Result<ContainerId> {
        let response = oneshot::channel();
        self.sender
            .send(docker::Msg::Create(option, response.0))
            .await
            .context("Unable to send docker::Msg::Create msg")?;
        Ok(response.1.await??)
    }
}
