use std::{net::Ipv4Addr, sync::Arc, time::Duration};

use log::{error, info};
use tokio::{
    io::copy,
    net::{TcpListener, TcpStream},
    sync::{mpsc::Sender, Mutex},
    time::{self, sleep, Instant},
};

use crate::docker::{self, Id};

const MAX_TRIES: usize = 100;

pub struct Proxy {
    // Which port to listen on for incomming connection.
    listen_port: u16,

    // Name of the docker image.
    image_name: String,
    // Internal port on which the challenge listens
    service_port: u16,

    // Port on which the container maps the service port
    // to host network.
    proxy_port: u16,

    docker_man: Sender<docker::Msg>,
}

impl Proxy {
    pub fn new(
        listen_port: u16,
        image_name: String,
        service_port: u16,
        proxy_port: u16,
        docker_man: Sender<docker::Msg>,
    ) -> Self {
        Self {
            listen_port,
            image_name,
            service_port,
            proxy_port,
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

        let option = docker::CreateOption {
            image_name: self.image_name.clone(),
            service_port: self.service_port,
            port: self.proxy_port,
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
                    let _ = self
                        .docker_man
                        .send(docker::Msg::Start(id.clone()))
                        .await
                        .map_err(|err| error!("Unable to send Msg to DockerMan {}", err));
                    tokio::spawn(Proxy::handle_connection(
                        stream,
                        tick.clone(),
                        self.proxy_port,
                    ));
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

    async fn handle_connection(mut upstream: TcpStream, tick: Tick, proxy_port: u16) {
        let mut no_of_try = MAX_TRIES;
        let instant = Instant::now();
        loop {
            if no_of_try == 0 {
                break;
            }
            no_of_try -= 1;

            match TcpStream::connect((Ipv4Addr::LOCALHOST, proxy_port)).await {
                Ok(mut proxy_stream) => {
                    info!(
                        "Proxied Connection after try: {} : {:?}",
                        MAX_TRIES - no_of_try,
                        instant.elapsed()
                    );

                    tick.reset().await;

                    let (mut upstream_r, mut upstream_w) = upstream.split();
                    let (mut proxy_r, mut proxy_w) = proxy_stream.split();

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
    container_id: Id,

    // Duration between every tick.
    delay: Duration,
    timer: Tick,
}

impl Timer {
    fn new(
        docker_man: Sender<docker::Msg>,
        delay: Duration,
        max_tick: usize,
        container_id: Id,
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
