use std::{
    collections::HashMap,
    fmt::Arguments,
    marker::Unpin,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::Context;
use std::hash::Hash;

use log::{error, info, warn};
use pin_project::pin_project;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use socket2::{Domain, Protocol, Socket, TcpKeepalive, Type};
use tokio::{
    io::{copy_bidirectional, AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc::{self, Receiver, Sender},
    time::{sleep, Instant},
};
use tokio_rustls::{
    rustls::{Certificate, PrivateKey, ServerConfig},
    server::TlsStream,
    TlsAcceptor,
};
use tokio_util::sync::CancellationToken;

use crate::{
    define_registry,
    docker::{self, ContainerId, Env},
    id::TypedId,
    tui::{self, LogInfo, TuiSender},
};

const MAX_TRIES: usize = 100;

enum SuperStream {
    Tcp(TcpStream),
    Tls(TlsStream<TcpStream>),
}

pub enum TlsMsg {
    Add(String, ProxyId),
    Remove(ProxyId),
}

// Listens for TLS stream and proxy the connection.
// TLS Listener can proxy multiple connection using
// SNI based routing, if a wildcard certificate is provided.
pub struct TlsListener {
    // Which port to listen on for incomming connection.
    listen_port: u16,

    // Containers Certificate
    cert: Vec<Certificate>,

    // Private Key for the certificate
    key: PrivateKey,

    // Host address of the wild card certificate
    host: String,

    // Mapping between SNI Host name and Proxy
    map: HashMap<String, ProxyId>,

    // Registry containing all active proxy.
    proxy_registry: ProxyRegistry,

    conn: (Sender<TlsMsg>, Receiver<TlsMsg>),
}

impl TlsListener {
    pub fn new(
        listen_port: u16,
        cert: Vec<Certificate>,
        key: PrivateKey,
        host: String,
        proxy_registry: ProxyRegistry,
    ) -> Self {
        let map = HashMap::new();
        let conn = mpsc::channel(10);
        Self {
            listen_port,
            cert,
            key,
            map,
            conn,
            host,
            proxy_registry,
        }
    }

    pub fn sender(&self) -> Sender<TlsMsg> {
        self.conn.0.clone()
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        info!("Starting TLSListener on Port: {}", self.listen_port);

        let socket_addr = SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(0, 0, 0, 0),
            self.listen_port,
        ));

        let socket = prepare_socket(socket_addr).context(format!(
            "Unable to create socket for listening on : {socket_addr}"
        ))?;

        let listener = std::net::TcpListener::from(socket);
        let listener = tokio::net::TcpListener::from_std(listener)
            .context("Unable to create Tokio TCP Listener from socket2 socket.")?;

        let server_config = ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(self.cert.clone(), self.key.clone())
            .context("Unable to create ServerConfig")?;

        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        loop {
            tokio::select! {
                Some(msg) = self.conn.1.recv() => {
                    self.handle_tls_msg(msg)
                }
                Ok((stream, socket)) = listener.accept() => {
                    self.handle_connection(&acceptor, stream, socket).await;
                }
            }
        }
    }

    fn handle_tls_msg(&mut self, msg: TlsMsg) {
        match msg {
            TlsMsg::Add(host, proxy_id) => {
                let sni = format!("{}.{}", host, self.host);
                info!("Added host<{}> for proxing by TLSListener", sni);
                self.map.insert(sni, proxy_id);
            }
            TlsMsg::Remove(proxy_id) => {
                // Remove the entry from the map
                self.map.retain(|_, value| *value != proxy_id);
            }
        }
    }
    async fn handle_connection(
        &mut self,
        acceptor: &TlsAcceptor,
        stream: TcpStream,
        socket: SocketAddr,
    ) {
        info!("Got Connection from: {socket}");
        match acceptor.accept(stream).await {
            Err(err) => {
                error!("Unable to accept TLS Stream: {err}");
            }
            Ok(stream) => {
                let (_, connection) = stream.get_ref();
                if let Some(sni) = connection.server_name() {
                    info!("Got Connection with SNI: {sni}");
                    if let Some(proxy_id) = self.map.get(sni).cloned() {
                        let stream = SuperStream::Tls(stream);
                        if let Some(proxy) = self.proxy_registry.get(&proxy_id).await {
                            let fut = async move {
                                let proxy = proxy;
                                proxy.tui_log(format_args!(
                                    "[{}] Got Connection from: {socket}",
                                    proxy.container_id
                                ));

                                proxy.run(socket, stream).await;

                                proxy.tui_log(format_args!(
                                    "[{}] Closed Connection from: {socket}",
                                    proxy.container_id
                                ));
                            };
                            tokio::spawn(fut);
                        } else {
                            // Unable to find the particular proxy in proxy_registry. Remove from the mapping.
                            self.map.retain(|_, value| *value != proxy_id);
                        }
                    } else {
                        warn!("Unable to find Proxy for SNI: {sni}");
                    }
                }
            }
        }
    }
}

// Listens for a TCP Stream an proxy the connection.
pub struct TcpListener {
    // Which port to listen on for incomming connection.
    listen_port: u16,
    // Proxy the connection.
    proxy: Arc<Proxy>,
    // CancellationToken for shutting down the Listener.
    cancel: CancellationToken,
}

impl TcpListener {
    pub fn new(listen_port: u16, proxy: Arc<Proxy>) -> Self {
        // Cancel the listener when the proxy is also cancelled.
        let cancel = proxy.cancellation_token.clone();
        Self {
            listen_port,
            proxy,
            cancel,
        }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        self.proxy.tui_log(format_args!(
            "[{}] Starting TCPListener on Port:\t{}",
            self.proxy.container_id, self.listen_port
        ));

        let socket_addr = SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(0, 0, 0, 0),
            self.listen_port,
        ));

        let socket = prepare_socket(socket_addr)?;

        let listener = std::net::TcpListener::from(socket);
        let listener = tokio::net::TcpListener::from_std(listener)
            .context("Unable to create Tokio TCP Listener from socket2 socket.")?;

        loop {
            tokio::select! {
                Ok((stream, socket))  = listener.accept() => {
                    self.proxy.tui_log(format_args!(
                        "[{}] Got Connection from: {socket}",
                        self.proxy.container_id
                    ));

                    let stream = SuperStream::Tcp(stream);
                    let proxy = self.proxy.clone();
                    let fut = async move {
                        proxy.run(socket, stream).await;
                        proxy.tui_log(format_args!( "[{}] Closed Connection from: {socket}", proxy.container_id));
                    };
                    tokio::spawn(fut);
                },
                _ = self.cancel.cancelled() => {
                    self.proxy.tui_log(format_args!("Stopping TCPListener on Port: {}", self.listen_port));
                    return Ok(());
                }
            }
        }
    }
}

fn prepare_socket(socket_addr: SocketAddr) -> anyhow::Result<Socket> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(10))
        .with_retries(3)
        .with_interval(Duration::from_secs(10));

    socket.set_tcp_keepalive(&keepalive)?;

    // setting TCP_USER_TIMEOUT as TCP_KEEPIDLE + TCP_KEEPINTVL * TCP_KEEPCNT
    // Using this blog as reference :
    // https://blog.cloudflare.com/when-tcp-sockets-refuse-to-die
    socket.set_tcp_user_timeout(Some(Duration::from_secs(40)))?;
    socket.set_nonblocking(true)?;
    socket.bind(&socket_addr.into())?;
    socket.listen(128)?;

    Ok(socket)
}

use crate::id::Registry;

pub type ProxyId = TypedId<Arc<Proxy>>;
define_registry!(ProxyRegistry, Arc<Proxy>);

// TODO: Arc around Proxy ?
#[derive(Clone)]
pub struct Proxy {
    // Internal Id of the container to which the traffic should be
    // proxied.
    container_id: ContainerId,
    // Port on which the container maps the service port
    proxy_port: u16,

    // Address on which the container starts
    proxy_host: String,

    // Environment variables for the container.
    env: Option<Env>,

    // Sender to communicate with DockerMan to start
    // containers when a new request is received.
    docker_man: Sender<docker::Msg>,

    // Sender to communicate with TUI.
    tui: Option<TuiSender>,

    // Tracks the number of active connection for this proxy.
    active_connection: ActiveConn,

    // Cancellation token for gracefully shutting down resources associated with the proxy.
    // Including the Connection Tracker.
    cancellation_token: CancellationToken,
}

impl Proxy {
    pub fn new(
        container_id: ContainerId,
        proxy_port: u16,
        proxy_host: String,
        env: Option<Env>,
        docker_man: Sender<docker::Msg>,
    ) -> Self {
        let cancellation_token = CancellationToken::new();
        let child_token = cancellation_token.child_token();
        let conn_track = ConnTrack::new(
            Duration::from_secs(60),
            docker_man.clone(),
            container_id.clone(),
            child_token,
        );

        let active_connection = conn_track.no_conn();
        tokio::spawn(conn_track.run());

        let tui = tui::TUI_SENDER.get().cloned();

        Self {
            container_id,
            proxy_port,
            proxy_host,
            docker_man,
            tui,
            env,
            active_connection,
            cancellation_token,
        }
    }

    // Does the necessary proxy level cleaup. Including stopping the container and informing TUI.
    // We have this as a seperate funtion because Async Drop is still not supported yet.
    pub async fn cleanup(&self) -> anyhow::Result<()> {
        let id = TypedId::<Proxy>::from(self);
        self.docker_man
            .send(docker::Msg::Stop(self.container_id.clone()))
            .await?;

        if let Some(sender) = &self.tui {
            sender.send(tui::Msg::Remove(id.value()))?;
        }

        // Stops the active connection counter.
        self.cancellation_token.cancel();
        Ok(())
    }

    pub fn tui_log(&self, fmt: Arguments<'_>) {
        let id = TypedId::<Proxy>::from(self);

        // Print the formatted string as log message
        let log = format!("{}", fmt);
        info!("{}", log);

        if let Some(sender) = &self.tui {
            let log = LogInfo {
                id: id.value(),
                log,
            };
            let _ = sender.send(tui::Msg::Log(log));
        }
    }

    async fn run(&self, socket: SocketAddr, super_stream: SuperStream) {
        let no_conn = self.active_connection.clone();
        let flag = self
            .env
            .as_ref()
            .map(|x| Vec::from(x.value.as_bytes()))
            .unwrap_or_default();

        let _ = self
            .docker_man
            .send(docker::Msg::Start(self.container_id.clone()))
            .await
            .map_err(|err| error!("Unable to send Msg to DockerMan {}", err));

        match super_stream {
            SuperStream::Tcp(s) => {
                tokio::spawn(Proxy::handle_connection(
                    s,
                    self.container_id.clone(),
                    flag.clone(),
                    no_conn.clone(),
                    self.proxy_host.clone(),
                    self.proxy_port,
                    socket,
                ));
            }
            SuperStream::Tls(s) => {
                tokio::spawn(Proxy::handle_connection(
                    s,
                    self.container_id.clone(),
                    flag.clone(),
                    no_conn.clone(),
                    self.proxy_host.clone(),
                    self.proxy_port,
                    socket,
                ));
            }
        }
    }

    async fn handle_connection<T>(
        upstream: T,
        container_id: ContainerId,
        flag: Vec<u8>,
        no_conn: ActiveConn,
        proxy_host: String,
        proxy_port: u16,
        socket: SocketAddr,
    ) where
        T: Unpin + AsyncRead + AsyncWrite,
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
                        "[{}] Proxied Connection after try: {} : {:?}",
                        container_id,
                        MAX_TRIES - no_of_try,
                        instant.elapsed()
                    );

                    // Increment the no of connection.
                    no_conn.inc();

                    // Set timeout for the stream.
                    let mut upstream = tokio_io_timeout::TimeoutStream::new(upstream);
                    upstream.set_read_timeout(Some(Duration::from_secs(120)));
                    let mut upstream = Box::pin(upstream);

                    // Enable this for transforming flags in the stream
                    // let mut proxy_stream = FlagTransformer::new(flag, proxy_stream);

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
                    no_conn.dec();
                    break;
                }
                Err(_) => {
                    // Sleep for 100ms before tyring to connect to proxy port
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }
        info!(
            "[{}] Connection closed for: {socket} elapsed time {:?}",
            container_id,
            instant.elapsed()
        );
    }
}

impl Hash for Proxy {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.container_id.hash(state);
        self.proxy_port.hash(state);
        self.proxy_host.hash(state);
        self.env.hash(state);
    }
}

impl PartialEq for Proxy {
    fn eq(&self, other: &Self) -> bool {
        self.container_id == other.container_id
            && self.proxy_port == other.proxy_port
            && self.proxy_host == other.proxy_host
            && self.env == other.env
    }
}

impl Eq for Proxy {}

#[derive(Clone)]
struct ActiveConn(Arc<AtomicU64>);

impl ActiveConn {
    fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }

    // Increments the number of active connections.
    // Should be called when a proxy connection is started.
    fn inc(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    // Decrementes the number of active connections.
    // Should be called when proxying of a connection is completed.
    fn dec(&self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }

    // Returns the number of active connections.
    fn load(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
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
    no_conn: ActiveConn,

    // determines the idle time after which the connection
    // closed
    timeout: Duration,

    // channel to dockerman
    docker_man: Sender<docker::Msg>,

    // id of the container that is being tracked
    container_id: ContainerId,

    // Stop the process with the token is cancelled.
    cancellation_token: CancellationToken,
}

impl ConnTrack {
    fn new(
        timeout: Duration,
        docker_man: Sender<docker::Msg>,
        container_id: ContainerId,
        cancellation_token: CancellationToken,
    ) -> Self {
        let no_conn = ActiveConn::new();
        let interval = Duration::from_secs(5);
        Self {
            no_conn,
            interval,
            timeout,
            docker_man,
            container_id,
            cancellation_token,
        }
    }

    fn no_conn(&self) -> ActiveConn {
        self.no_conn.clone()
    }

    async fn run(self) {
        let mut last_activity = Instant::now();

        loop {
            // Stop the process with the token is cancelled.
            if self.cancellation_token.is_cancelled() {
                return;
            }
            let no_conn = self.no_conn.load();

            if no_conn != 0 {
                info!("Total No of connection active : {}", no_conn);
            }

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

                    // Only send the next message after an entire idle period.
                    // The container takes some time to shutdown properly. This is to make sure
                    // we don't send multiple stop signals within that time.
                    last_activity = Instant::now();
                }
            }

            sleep(self.interval).await;
        }
    }
}

#[pin_project]
struct FlagTransformer<T> {
    from: Vec<u8>,
    to: Vec<u8>,
    #[pin]
    stream: T,
}

impl<T> FlagTransformer<T>
where
    T: Unpin + AsyncRead + AsyncWrite,
{
    fn new(from: Vec<u8>, stream: T) -> Self {
        let rng = thread_rng();
        let to: Vec<u8> = rng.sample_iter(&Alphanumeric).take(from.len()).collect();

        Self { from, stream, to }
    }
}

impl<T> AsyncRead for FlagTransformer<T>
where
    T: AsyncRead + Unpin,
{
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.project();
        let result = AsyncRead::poll_read(this.stream, cx, buf);

        if result.is_ready() && !this.from.is_empty() {
            let buffer = buf.filled_mut();

            let mut i = 0;
            let len = this.from.len();
            while i + len <= buffer.len() {
                if &buffer[i..(i + len)] == this.from {
                    buffer[i..(i + len)].copy_from_slice(this.to);
                    // Replace only once.
                    break;
                } else {
                    i += 1;
                }
            }
        }

        result
    }
}

impl<T> AsyncWrite for FlagTransformer<T>
where
    T: AsyncWrite + Unpin,
{
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        let this = self.project();
        AsyncWrite::poll_write(this.stream, cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        let this = self.project();
        AsyncWrite::poll_flush(this.stream, cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        let this = self.project();
        AsyncWrite::poll_shutdown(this.stream, cx)
    }
}
