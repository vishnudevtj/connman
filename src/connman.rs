use std::sync::Arc;

use crate::docker::Env;
use crate::docker::ImageOption;

use crate::docker::ContainerId;
use crate::docker::CreateOption;
use crate::docker::{DockerMan, ImageId};

use crate::docker;
use crate::proxy::Proxy;
use crate::proxy::TlsMsg;
use crate::proxy::{TcpListener, TlsListener};

use highway::HighwayHasher;
use highway::Key;
use log::error;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;

use rand::rngs::StdRng;

use rand::SeedableRng;
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
    pub image: ImageId,
    // Environment variables for the container.
    pub env: Option<Env>,
}

pub struct TcpPorxy {
    // host address
    pub host: String,
    // Port on proxy host to listen.
    pub listen_port: u16,
    // Id of the image to deploy for proxying.
    pub image: ImageId,
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

pub enum Msg {
    // Deploys a challenge and returns the URL to access it.
    TlsProxy(TlsProxy, oneshot::Sender<Result<Url>>),
    // Deploys a TCP challenge and retuns host:port.
    TcpPorxy(TcpPorxy, oneshot::Sender<Result<Url>>),
    // Register a container image on all available docker backend.
    RegisterImage(ImageOption, oneshot::Sender<Result<ImageId>>),
}

pub struct ConnmanBuilder {
    // Contains TLS certificate and private key for SNI based routing.
    tls: Option<(Vec<Certificate>, PrivateKey)>,
    // List of docker backend.
    docker: Vec<DockerMan>,
}

impl ConnmanBuilder {
    pub fn new() -> Self {
        Self {
            tls: None,
            docker: Vec::new(),
        }
    }

    pub fn with_tls(mut self, cert: Vec<Certificate>, key: PrivateKey) -> Self {
        self.tls = Some((cert, key));
        self
    }

    pub fn with_docker(mut self, addr: String, port: u16) -> Result<Self> {
        let docker_man = DockerMan::new(addr, port)?;
        self.docker.push(docker_man);
        Ok(self)
    }

    pub fn build(self) -> anyhow::Result<Connman> {
        if self.docker.len() == 0 {
            return Err(anyhow!("No Docker backend configured!"));
        }
        let tls = self.tls.map(|x| {
            let listener = TlsListener::new(TLS_PORT, x.0, x.1);
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
        Ok(Connman {
            tls,
            docker,
            sender: conn.0,
            receiver: Some(conn.1),
        })
    }
}

pub struct Connman {
    // Channel to communicate with TLSListener.
    tls: Option<Sender<TlsMsg>>,

    // List of all available docker backend.
    docker: Vec<DockerBackEnd>,

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
                let r = docker.register_image(image_option).await;
                let _ = result.send(r);
            }
            Msg::TlsProxy(tls_option, result) => {
                let r = self.handle_tls_proxy(tls_option).await;
                let _ = result.send(r);
            }
            Msg::TcpPorxy(tcp_option, result) => {
                let r = self.handle_tcp_proxy(tcp_option).await;
                let _ = result.send(r);
            }
        }
    }

    pub fn sender(&self) -> Sender<Msg> {
        self.sender.clone()
    }

    pub fn receiver(&mut self) -> Option<Receiver<Msg>> {
        self.receiver.take()
    }

    async fn handle_tcp_proxy(&self, tcp_option: TcpPorxy) -> Result<Url> {
        let docker = self.get_docker_man();
        let proxy_host = docker.host.clone();

        let docker_port = docker
            .port_range
            .aquire_port()
            .await
            .ok_or(anyhow!("No port left on docker host"))?;

        let _rng = {
            let rng = rand::thread_rng();
            StdRng::from_rng(rng)?
        };
        use highway::HighwayHash;
        let hash_key = Key(HASH_KEY);
        let mut hasher = HighwayHasher::new(hash_key);
        hasher.append(&tcp_option.image.into_inner().to_le_bytes());
        hasher.append(&tcp_option.listen_port.to_le_bytes());
        if let Some(env) = tcp_option.env.as_ref() {
            hasher.append(env.key.as_bytes());
            hasher.append(env.value.as_bytes());
        };
        let name = hasher.finalize64().to_string();

        let create_option = CreateOption {
            image_id: tcp_option.image,
            container_name: name,
            env: tcp_option.env.clone(),
            port: docker_port,
        };

        let container_id = docker.create_container(create_option).await?;
        let proxy_port = tcp_option.listen_port;
        let proxy = Proxy::new(
            container_id,
            proxy_port,
            proxy_host.clone(),
            tcp_option.env.clone(),
            docker.sender(),
        );

        let listener = TcpListener::new(proxy_port, proxy);
        let fut = async {
            listener
                .run()
                .await
                .map_err(|err| error!("Unable to start TcpListener: {err}"))
        };
        tokio::spawn(fut);

        let url = Url {
            host: tcp_option.host.clone(),
            port: proxy_port,
        };
        Ok(url)
    }

    async fn handle_tls_proxy(&self, tls_option: TlsProxy) -> Result<Url> {
        let docker = self.get_docker_man();
        let port = docker
            .port_range
            .aquire_port()
            .await
            .ok_or(anyhow!("No port left on docker host"))?;

        match &self.tls {
            Some(sender) => {
                let docker = self.get_docker_man();

                use highway::HighwayHash;
                let hash_key = Key(HASH_KEY);
                let mut hasher = HighwayHasher::new(hash_key);
                hasher.append(&tls_option.image.into_inner().to_le_bytes());
                hasher.append(tls_option.host.as_bytes());
                if let Some(env) = tls_option.env.as_ref() {
                    hasher.append(env.key.as_bytes());
                    hasher.append(env.value.as_bytes());
                };
                let name = hasher.finalize64().to_string();

                let create_option = CreateOption {
                    image_id: tls_option.image,
                    container_name: name,
                    env: tls_option.env.clone(),
                    port,
                };

                let container_id = docker.create_container(create_option).await?;

                let proxy = Proxy::new(
                    container_id,
                    port,
                    docker.host.clone(),
                    tls_option.env.clone(),
                    docker.sender(),
                );

                sender
                    .send(TlsMsg::Add(tls_option.host.clone(), proxy))
                    .await?;
                let url = Url {
                    host: tls_option.host.clone(),
                    port: TLS_PORT,
                };
                Ok(url)
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
        for (idx, used) in inner.used.iter_mut().enumerate() {
            if *used != true {
                *used = true;
                return Some(inner.start + idx as u16);
            }
        }
        None
    }

    pub async fn _release_port(&self, port: u16) {
        let mut inner = self.inner.lock().await;
        if (port > inner._end) {
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
    async fn register_image(&self, image_option: ImageOption) -> Result<ImageId> {
        let response = oneshot::channel();
        self.sender
            .send(docker::Msg::Register(image_option, response.0))
            .await
            .context("Unable to send docker::Msg::Resgister msg")?;
        Ok(response.1.await??)
    }
    fn sender(&self) -> Sender<docker::Msg> {
        self.sender.clone()
    }
    async fn create_container(&self, option: CreateOption) -> Result<ContainerId> {
        let response = oneshot::channel();
        self.sender
            .send(docker::Msg::Create(option, response.0))
            .await
            .context("Unable to send docker::Msg::Create msg")?;
        Ok(response.1.await??)
    }
}
