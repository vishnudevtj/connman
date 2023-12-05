pub mod docker;
pub mod proxy;

use docker::ContainerId;
use docker::CreateOption;
use docker::{DockerMan, Env, ImageId, ImageOption};

use proxy::Proxy;
use proxy::{TcpListener, TlsListener};

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use rand::distributions::Alphanumeric;
use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;
use tokio::sync::{
    mpsc::{Receiver, Sender},
    oneshot,
};
use tokio_rustls::rustls::{Certificate, PrivateKey};

pub struct TlsProxy {
    // SNI address for proxying.
    pub host: String,
    // Id of the image to deploy for proxying.
    pub image: ImageId,
    // Environment variables for the container.
    pub env: Option<Env>,
}

pub struct TcpPorxy {
    // Port on proxy host to listen.
    pub listen_port: u16,
    // Id of the image to deploy for proxying.
    pub image: ImageId,
    // Environment variables for the container.
    pub env: Option<Env>,
}

pub enum Msg {
    // Deploys a challenge and returns the URL to access it.
    TlsProxy(TlsProxy, oneshot::Sender<Result<String>>),
    // Deploys a TCP challenge and retuns host:port.
    TcpPorxy(TcpPorxy, oneshot::Sender<Result<String>>),
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
            let listener = TlsListener::new(443, x.0, x.1);
            let map = listener.sender();
            tokio::spawn(listener.run());
            map
        });

        let docker = self
            .docker
            .into_iter()
            .map(|docker| {
                let sender = docker.sender();
                let host = docker.host();
                let port = docker.port();
                let port_range = PortRange::new(10_000, 11_000);
                let back_end = DockerBackEnd {
                    host,
                    port,
                    sender,
                    port_range,
                };
                tokio::spawn(docker.run());
                back_end
            })
            .collect();

        let conn = tokio::sync::mpsc::channel(10);
        Ok(Connman { tls, docker, conn })
    }
}

pub struct Connman {
    // Channel to communicate with TLSListener.
    tls: Option<Sender<proxy::TlsMsg>>,

    // List of all available docker backend.
    docker: Vec<DockerBackEnd>,

    conn: (Sender<Msg>, Receiver<Msg>),
}

impl Connman {
    pub async fn run(mut self) {
        while let Some(msg) = self.conn.1.recv().await {
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
    }

    pub fn sender(&self) -> Sender<Msg> {
        self.conn.0.clone()
    }

    async fn handle_tcp_proxy(&mut self, tcp_option: TcpPorxy) -> Result<String> {
        let docker = self.get_docker_man_mut();
        let proxy_host = docker.host.clone();
        let proxy_port = docker
            .port_range
            .aquire_port()
            .ok_or(anyhow!("No port left on docker host"))?;

        let rng = {
            let rng = rand::thread_rng();
            StdRng::from_rng(rng)?
        };
        let name: Vec<u8> = rng.sample_iter(&Alphanumeric).take(10).collect();
        let name = String::from_utf8(name)?;

        let create_option = CreateOption {
            image_id: tcp_option.image,
            container_name: name,
            env: tcp_option.env.clone(),
            port: proxy_port,
        };

        let container_id = docker.create_container(create_option).await?;

        let proxy = Proxy::new(
            container_id,
            proxy_port,
            proxy_host.clone(),
            tcp_option.env.clone(),
            docker.sender(),
        );

        let listen_port = tcp_option.listen_port;
        let listener = TcpListener::new(listen_port, proxy);
        tokio::spawn(listener.run());

        let host = format!("0.0.0.0:{}", listen_port);
        Ok(host)
    }

    async fn handle_tls_proxy(&mut self, tls_option: TlsProxy) -> Result<String> {
        let docker = self.get_docker_man_mut();
        let port = docker
            .port_range
            .aquire_port()
            .ok_or(anyhow!("No port left on docker host"))?;

        match &self.tls {
            Some(sender) => {
                let docker = self.get_docker_man();
                let rng = {
                    let rng = rand::thread_rng();
                    StdRng::from_rng(rng)?
                };
                let name: Vec<u8> = rng.sample_iter(&Alphanumeric).take(10).collect();
                let name = String::from_utf8(name)?;

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
                    .send(proxy::TlsMsg::Add(tls_option.host.clone(), proxy))
                    .await?;
                let host = format!("{}", tls_option.host);
                Ok(host)
            }
            None => Err(anyhow!("TLS Listener not configured")),
        }
    }

    fn get_docker_man(&self) -> &DockerBackEnd {
        let rand: usize = rand::random();
        &self.docker[rand % self.docker.len()]
    }

    fn get_docker_man_mut(&mut self) -> &mut DockerBackEnd {
        let len = self.docker.len();
        let rand: usize = rand::random();
        &mut self.docker[rand % len]
    }
}

struct PortRange {
    start: u16,
    end: u16,
    used: Vec<bool>,
}

impl PortRange {
    fn new(start: u16, end: u16) -> Self {
        assert!(end > start);
        let used = vec![false; (end - start) as usize];
        Self { start, end, used }
    }
    fn aquire_port(&mut self) -> Option<u16> {
        for (idx, used) in self.used.iter_mut().enumerate() {
            if *used != true {
                *used = true;
                return Some(self.start + idx as u16);
            }
        }
        None
    }
    fn release_port(&mut self, port: u16) {
        assert!(port < self.end);
        let id = port - self.start;
        self.used[id as usize] = false;
    }
}

// Structure which contains details about a specific docker backend.
struct DockerBackEnd {
    host: String,
    port: u16,
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
