use std::{
    net::{Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use log::{error, info};
use tokio::{
    io::{copy, split},
    net::{TcpListener, TcpStream},
    pin,
    sync::{mpsc::Sender, Mutex},
    time::{self, interval, sleep, Instant},
};
use tokio_rustls::{
    rustls::{Certificate, PrivateKey, ServerConfig},
    server::TlsStream,
    TlsAcceptor,
};

use crate::docker::{self, ContainerId};

const MAX_TRIES: usize = 100;

pub struct Proxy {
    // Which port to listen on for incomming connection.
    listen_port: u16,

    // Name of the docker image.
    image_name: String,
    // Internal port on which the challenge listens
    service_port: u16,

    // Port on which the container maps the service port
    proxy_port: u16,

    // Address on which the container starts
    proxy_host: String,

    // Container name
    container_name: String,

    // Containers Certificate
    cert: Vec<Certificate>,

    // Private Key for the certificate
    key: PrivateKey,

    docker_man: Sender<docker::Msg>,
}

impl Proxy {
    pub fn new(
        listen_port: u16,
        image_name: String,
        service_port: u16,
        proxy_port: u16,
        proxy_host: String,
        container_name: String,
        cert: Vec<Certificate>,
        key: PrivateKey,
        docker_man: Sender<docker::Msg>,
    ) -> Self {
        Self {
            listen_port,
            image_name,
            service_port,
            proxy_port,
            proxy_host,
            container_name,
            cert,
            key,
            docker_man,
        }
    }

    pub async fn run(&self) {
        let socket_addr = format!("0.0.0.0:{}", self.listen_port);
        let listener = TcpListener::bind(&socket_addr)
            .await
            .map_err(|err| error!("Unable to Listen on addr: {} : {}", socket_addr, err))
            .unwrap();

        info!(
            "Listening on socker for image {} :  {socket_addr}",
            self.image_name
        );

        let server_config = ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(self.cert.clone(), self.key.clone())
            .map_err(|err| error!("Unable to create server_config : {err}"))
            .unwrap();

        let acceptor = TlsAcceptor::from(Arc::new(server_config));

        let option = docker::CreateOption {
            image_name: self.image_name.clone(),
            service_port: self.service_port,
            port: self.proxy_port,
            container_name: self.container_name.clone(),
        };

        let response = tokio::sync::oneshot::channel();
        let _ = self
            .docker_man
            .send(docker::Msg::Create(option, response.0))
            .await
            .map_err(|err| error!("Unable to send Msg to DockerMan: {}", err));

        let response = response.1.await;
        match response {
            Ok(Ok(id)) => {
                let delay = Duration::from_secs(10);
                let timer = Timer::new(self.docker_man.clone(), delay, 6, id.clone());
                let tick = timer.tick();
                tokio::spawn(timer.run());

                while let Ok((stream, socket)) = listener.accept().await {
                    info!("Got Connection from : {socket}");
                    let acceptor = acceptor.clone();
                    let stream = acceptor.accept(stream).await;
                    match stream {
                        Ok(stream) => {
                            let _ = self
                                .docker_man
                                .send(docker::Msg::Start(id.clone()))
                                .await
                                .map_err(|err| error!("Unable to send Msg to DockerMan {}", err));
                            tokio::spawn(Proxy::handle_connection(
                                stream,
                                tick.clone(),
                                self.proxy_host.clone(),
                                self.proxy_port,
                                socket,
                            ));
                        }
                        Err(err) => {
                            error!("Unable to accept TLS Stream : {err}");
                        }
                    }
                }
            }
            Ok(Err(err)) => {
                error!("Unable to create container: {}", err);
            }
            Err(_) => {
                error!("Error while receiving message from DockerMan")
            }
        }
    }

    async fn handle_connection(
        mut upstream: TlsStream<TcpStream>,
        tick: Tick,
        proxy_host: String,
        proxy_port: u16,
        socket: SocketAddr,
    ) {
        let mut no_of_try = MAX_TRIES;
        let instant = Instant::now();
        loop {
            if no_of_try == 0 {
                break;
            }
            no_of_try -= 1;

            match TcpStream::connect((proxy_host.as_str(), proxy_port)).await {
                Ok(mut proxy_stream) => {
                    info!(
                        "Proxied Connection after try: {} : {:?}",
                        MAX_TRIES - no_of_try,
                        instant.elapsed()
                    );

                    tick.reset().await;

                    let (mut upstream_r, mut upstream_w) = split(upstream);
                    let (mut proxy_r, mut proxy_w) = split(proxy_stream);

                    // let mut proxy_r = tokio_io_timeout::TimeoutReader::new(proxy_r);
                    // proxy_r.set_timeout(Some(Duration::from_secs(10)));
                    // let mut proxy_r = Box::pin(proxy_r);

                    // let mut proxy_w = tokio_io_timeout::TimeoutWriter::new(proxy_w);
                    // proxy_w.set_timeout(Some(Duration::from_secs(10)));
                    // let mut proxy_w = Box::pin(proxy_w);

                    let download = copy(&mut upstream_r, &mut proxy_w);
                    let upload = copy(&mut proxy_r, &mut upstream_w);

                    if let Err(e) = tokio::try_join!(download, upload) {
                        error!("Error in data transfer: {:?}", e);
                    }
                    break;
                }
                Err(_) => {
                    // Sleep for 100ms before tyring to connect to proxy port
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }
        info!(
            "Connection closed for: {socket} elapsed time {:?}",
            instant.elapsed()
        );
    }
}

#[derive(Clone)]
struct Tick {
    // Max value to tick from
    max: usize,
    current_value: Arc<Mutex<usize>>,
}

impl Tick {
    fn new(max: usize) -> Self {
        let lock = Arc::new(Mutex::new(max));
        Self {
            max,
            current_value: lock,
        }
    }

    async fn reset(&self) {
        let mut value = self.current_value.lock().await;
        *value = self.max;
    }

    async fn tick(&self) -> usize {
        let mut lock = self.current_value.lock().await;
        let current_value = *lock;
        if current_value != 0 {
            *lock -= 1
        }
        current_value
    }
}

struct Timer {
    docker_man: Sender<docker::Msg>,
    container_id: ContainerId,

    // Duration between every tick.
    delay: Duration,
    timer: Tick,
}

impl Timer {
    fn new(
        docker_man: Sender<docker::Msg>,
        delay: Duration,
        max_tick: usize,
        container_id: ContainerId,
    ) -> Self {
        let tick = Tick::new(max_tick);
        Self {
            docker_man,
            delay,
            timer: tick,
            container_id,
        }
    }

    fn tick(&self) -> Tick {
        self.timer.clone()
    }

    async fn run(self) {
        let mut interval = time::interval(self.delay);
        loop {
            interval.tick().await;

            let tick = self.timer.tick().await;
            if tick == 0 {
                let _ = self
                    .docker_man
                    .send(docker::Msg::Stop(self.container_id.clone()))
                    .await
                    .map_err(|err| error!("Unable to send message to DockerMan: {}", err));
            }
        }
    }
}
