use std::{
    net::{Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use log::{error, info, warn};
use tokio::{
    io::{copy, copy_bidirectional, split, AsyncRead, AsyncWrite, AsyncWriteExt},
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

enum SuperStream {
    Tcp(TcpStream),
    Tls(TlsStream<TcpStream>),
}

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
    cert: Option<Vec<Certificate>>,

    // Private Key for the certificate
    key: Option<PrivateKey>,

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
        cert: Option<Vec<Certificate>>,
        key: Option<PrivateKey>,
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

        let mut acceptor = None;
        if let (Some(cert), Some(key)) = (self.cert.clone(), self.key.clone()) {
            let server_config = ServerConfig::builder()
                .with_safe_defaults()
                .with_no_client_auth()
                .with_single_cert(cert, key)
                .map_err(|err| error!("Unable to create server_config : {err}"))
                .unwrap();
            acceptor = Some(TlsAcceptor::from(Arc::new(server_config)));
        }

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
                let conn_tracker =
                    ConnTrack::new(Duration::from_secs(60), self.docker_man.clone(), id.clone());
                let no_conn = conn_tracker.no_conn();
                tokio::spawn(conn_tracker.run());

                while let Ok((stream, socket)) = listener.accept().await {
                    info!("Got Connection from : {socket}");
                    let super_stream = if let Some(acceptor) = acceptor.clone() {
                        match acceptor.accept(stream).await {
                            Err(err) => {
                                error!("Unable to accept TLS Stream : {err}");
                                continue;
                            }
                            Ok(stream) => SuperStream::Tls(stream),
                        }
                    } else {
                        SuperStream::Tcp(stream)
                    };

                    let _ = self
                        .docker_man
                        .send(docker::Msg::Start(id.clone()))
                        .await
                        .map_err(|err| error!("Unable to send Msg to DockerMan {}", err));
                    match super_stream {
                        SuperStream::Tcp(s) => {
                            tokio::spawn(Proxy::handle_connection(
                                s,
                                no_conn.clone(),
                                self.proxy_host.clone(),
                                self.proxy_port,
                                socket,
                            ));
                        }
                        SuperStream::Tls(s) => {
                            tokio::spawn(Proxy::handle_connection(
                                s,
                                no_conn.clone(),
                                self.proxy_host.clone(),
                                self.proxy_port,
                                socket,
                            ));
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

    async fn handle_connection<T>(
        mut upstream: T,
        no_conn: Arc<AtomicU64>,
        proxy_host: String,
        proxy_port: u16,
        socket: SocketAddr,
    ) where
        T: std::marker::Unpin + AsyncRead + AsyncWrite,
    {
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

                    // Increment the no of connection.
                    no_conn.fetch_add(1, Ordering::SeqCst);

                    match copy_bidirectional(&mut upstream, &mut proxy_stream).await {
                        Ok((_to_egress, _to_ingress)) => {
                            // info!(
                            //     "Connection {socket} ended gracefully ({to_egress} bytes from client, {to_ingress} bytes from server)"
                            // );
                        }
                        Err(err) => {
                            warn!("Error while proxying: {socket} : {err}");
                        }
                    }

                    let _ = upstream.flush().await.map_err(|err| {
                        warn!("Unable to flush upstream of socket : {socket} : {err}")
                    });

                    let _ = proxy_stream
                        .flush()
                        .await
                        .map_err(|err| warn!("Unable to flush proxy of socket: {socket} {err}"));

                    // Decrement the no of connections.
                    no_conn.fetch_sub(1, Ordering::SeqCst);
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

// This handles the trackking of connection
// and stops the container after a period of idle
// time
struct ConnTrack {
    // determines how much time the runner should sleep
    // before checking for the number of active connection.
    interval: Duration,

    // tracks the number of active connection.
    no_conn: Arc<AtomicU64>,

    // determines the idle time after which the connection
    // closed
    timeout: Duration,

    // channel to dockerman
    docker_man: Sender<docker::Msg>,

    // if of the container that is being tracked
    container_id: ContainerId,
}

impl ConnTrack {
    fn new(timeout: Duration, docker_man: Sender<docker::Msg>, container_id: ContainerId) -> Self {
        let no_conn = Arc::new(AtomicU64::default());
        let interval = Duration::from_secs(5);
        Self {
            no_conn,
            interval,
            timeout,
            docker_man,
            container_id,
        }
    }

    fn no_conn(&self) -> Arc<AtomicU64> {
        self.no_conn.clone()
    }

    async fn run(self) {
        let mut last_activity = Instant::now();

        loop {
            let no_conn = self.no_conn.load(Ordering::SeqCst);
            info!("Total No of connection active : {}", no_conn);
            if no_conn > 0 {
                last_activity = Instant::now();
            } else {
                let idle = last_activity.elapsed();
                if idle >= self.timeout {
                    let _ = self
                        .docker_man
                        .send(docker::Msg::Stop(self.container_id.clone()))
                        .await
                        .map_err(|err| error!("Unable to send message to DockerMan: {}", err));
                }
            }

            sleep(self.interval).await;
        }
    }
}
