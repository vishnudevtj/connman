use std::net::SocketAddr;
use std::sync::Arc;

use log::{error, info};
use rusqlite::{Connection, OpenFlags};
use tokio::sync::mpsc::Sender;
use tokio::sync::{oneshot, Mutex};
use tonic::{transport::Server, Request, Response, Status};

use anyhow::anyhow;
pub mod grpc {
    tonic::include_proto!("connman");
}

use grpc::conn_man_server::{ConnMan, ConnManServer};
use grpc::{
    add_proxy_response, remove_proxy_response, AddProxyRequest, AddProxyResponse,
    AddTlsListenerRequest, AddTlsListenerResponse, ChallengeType, Proxy, RemoveProxyRequest,
    RemoveProxyResponse,
};

use crate::connman::{self, PortRange, TcpProxy};
use crate::docker::{Env, ImageId, ImageOption, ImageRegistry};
use crate::proxy::ProxyId;

use self::grpc::{register_image_response, RegisterImageRequest, RegisterImageResponse};

#[derive(serde::Serialize, serde::Deserialize, Debug)]
enum RequestType {
    AddTlsListener(AddTlsListenerRequest),
    RegisterImage(RegisterImageRequest),
    // Request and host_port
    AddProxy(AddProxyRequest, u16),
    RemoveProxy(RemoveProxyRequest),
}

struct Log {
    db: Arc<Mutex<rusqlite::Connection>>,
}

impl Log {
    fn new(path: String) -> anyhow::Result<Self> {
        let db = rusqlite::Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        Self::initialize(&db)?;
        let db = Arc::new(Mutex::new(db));
        Ok(Self { db })
    }

    fn initialize(conn: &Connection) -> anyhow::Result<()> {
        // Create the necessary table for storing the message logs
        conn.execute(
            "CREATE TABLE IF NOT EXISTS log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                value TEXT NOT NULL
            )",
            (),
        )?;
        Ok(())
    }

    async fn add(&self, request: RequestType) -> anyhow::Result<()> {
        let request = serde_json::to_string(&request)?;
        let conn = self.db.lock().await;
        conn.execute("INSERT INTO log (value) VALUES (?1)", &[&request])?;
        Ok(())
    }

    async fn messages(&self) -> anyhow::Result<Vec<RequestType>> {
        let conn = self.db.lock().await;
        let mut stmt = conn.prepare("SELECT value FROM log")?;
        let mut rows = stmt.query([])?;
        let mut messages = Vec::with_capacity(1000);
        while let Some(row) = rows.next()? {
            let request: String = row.get(0)?;
            let request = serde_json::from_str(&request)?;
            messages.push(request);
        }
        Ok(messages)
    }
}

pub struct ImplConnMan {
    host_port: PortRange,
    connman: Sender<connman::Msg>,
    image_registry: ImageRegistry,
    log: Log,
}

impl ImplConnMan {
    fn new(
        connman: Sender<connman::Msg>,
        db: Log,
        image_registry: ImageRegistry,
    ) -> anyhow::Result<Self> {
        let host_port = PortRange::new(10_000, 30_000);
        Ok(Self {
            connman,
            image_registry,
            host_port,
            log: db,
        })
    }
}

#[tonic::async_trait]
impl ConnMan for ImplConnMan {
    async fn add_tls_listener(
        &self,
        request: Request<AddTlsListenerRequest>,
    ) -> Result<Response<AddTlsListenerResponse>, Status> {
        todo!();
    }

    async fn register_image(
        &self,
        request: Request<RegisterImageRequest>,
    ) -> Result<Response<RegisterImageResponse>, Status> {
        let request: RegisterImageRequest = request.into_inner();

        // Add the request to the log
        let r = RequestType::RegisterImage(request.clone());
        let _ = self
            .log
            .add(r)
            .await
            .map_err(|err| error!("Unable to add register_image log: {}", err));

        let res = _register_image(&self.connman, request)
            .await
            .map_err(|err| {
                let response = RegisterImageResponse {
                    ok: false,
                    response: Some(register_image_response::Response::Error(err.to_string())),
                };

                Response::new(response)
            })
            .map(|id| {
                let response = RegisterImageResponse {
                    ok: false,
                    response: Some(register_image_response::Response::Id(id.value())),
                };

                Response::new(response)
            });

        match res {
            Ok(res) => return Ok(res),
            Err(res) => return Ok(res),
        }
    }

    async fn add_proxy(
        &self,
        request: Request<AddProxyRequest>,
    ) -> Result<Response<AddProxyResponse>, Status> {
        let request: AddProxyRequest = request.into_inner();

        // TODO: unwrap
        let host_port = { self.host_port.aquire_port().await.unwrap() };

        // Add the request to the log
        let r = RequestType::AddProxy(request.clone(), host_port);
        let _ = self
            .log
            .add(r)
            .await
            .map_err(|err| error!("Unable to add add_proxy log: {}", err));

        let res = _add_proxy(
            &self.connman,
            host_port,
            request,
            self.image_registry.clone(),
        )
        .await
        .map_err(|err| {
            let resp = AddProxyResponse {
                ok: false,
                response: Some(add_proxy_response::Response::Error(err.to_string())),
            };
            Response::new(resp)
        })
        .map(|proxy| {
            let response = AddProxyResponse {
                ok: true,
                response: Some(add_proxy_response::Response::Proxy(proxy)),
            };
            Response::new(response)
        });

        match res {
            Ok(res) => return Ok(res),
            Err(res) => return Ok(res),
        }
    }

    async fn remove_proxy(
        &self,
        request: Request<RemoveProxyRequest>,
    ) -> Result<Response<RemoveProxyResponse>, Status> {
        let request: RemoveProxyRequest = request.into_inner();

        // Add the request to the log
        let r = RequestType::RemoveProxy(request.clone());
        let _ = self
            .log
            .add(r)
            .await
            .map_err(|err| error!("Unable to add add_proxy log: {}", err));

        let res = _stop_proxy(&self.connman, request)
            .await
            .map_err(|err| {
                let resp = RemoveProxyResponse {
                    ok: false,
                    response: Some(remove_proxy_response::Response::Error(err.to_string())),
                };
                Response::new(resp)
            })
            .map(|proxy| {
                let response = RemoveProxyResponse {
                    ok: true,
                    response: Some(remove_proxy_response::Response::Id(proxy.into_inner())),
                };
                Response::new(response)
            });

        match res {
            Ok(res) => return Ok(res),
            Err(res) => return Ok(res),
        }
    }
}

async fn _stop_proxy(
    connman: &Sender<connman::Msg>,
    request: RemoveProxyRequest,
) -> anyhow::Result<ProxyId> {
    let proxy_id = ProxyId::new(request.id);
    let channel = oneshot::channel();
    let msg = connman::Msg::StopProxy(proxy_id, channel.0);
    connman.send(msg).await?;
    let proxy_id = channel.1.await??;
    Ok(proxy_id)
}

async fn _register_image(
    connman: &Sender<connman::Msg>,
    request: RegisterImageRequest,
) -> anyhow::Result<ImageId> {
    // Register the image.
    let image_option = ImageOption {
        always_pull: true,
        service_port: request.port as u16,
        name: request.image,
        tag: request.tag,
        credentials: None,
    };

    let channel = oneshot::channel();
    let msg = connman::Msg::RegisterImage(image_option, channel.0);
    connman.send(msg).await?;
    let image_id = channel.1.await??;

    Ok(image_id)
}

async fn _add_proxy(
    connman: &Sender<connman::Msg>,
    host_port: u16,
    request: AddProxyRequest,
    image_registry: ImageRegistry,
) -> anyhow::Result<Proxy> {
    let image_id = image_registry
        .is_valid(request.id)
        .await
        .ok_or_else(|| anyhow!("Invalid ImageId"))?;

    let env = if let (Some(key), Some(value)) = (request.env_key, request.env_value) {
        Some(Env { key, value })
    } else {
        None
    };

    let channel = oneshot::channel();
    let tcp_proxy = TcpProxy {
        // TODO: ??
        host: "0.0.0.0".to_string(),
        listen_port: host_port,
        image_id,
        env,
    };

    let msg = connman::Msg::TcpProxy(tcp_proxy, channel.0);
    connman.send(msg).await?;
    let result = channel.1.await??;
    let url = result.1;
    let proxy_id = result.0.into_inner();

    Ok(Proxy {
        proxy_id,
        host: url.host,
        port: url.port.to_string(),
    })
}

async fn reply_log(
    requests: Vec<RequestType>,
    connman: &Sender<connman::Msg>,
    image_registry: ImageRegistry,
) {
    for request in requests {
        match request {
            RequestType::RegisterImage(msg) => {
                let connman = connman.clone();
                let fut = async move {
                    let res = _register_image(&connman, msg).await;
                };
                tokio::spawn(fut);
            }
            RequestType::AddProxy(msg, host_port) => {
                let connman = connman.clone();
                let image_registry = image_registry.clone();

                let fut = async move {
                    let res = _add_proxy(&connman, host_port, msg, image_registry).await;
                };
                tokio::spawn(fut);
            }
            RequestType::RemoveProxy(msg) => {
                let connman = connman.clone();
                let fut = async move {
                    let res = _stop_proxy(&connman, msg).await;
                };
                tokio::spawn(fut);
            }
            _ => {
                todo!()
            }
        }
    }
}

pub async fn start_grpc(
    addr: SocketAddr,
    docker: Sender<connman::Msg>,
    image_registry: ImageRegistry,
    database_path: String,
) -> anyhow::Result<()> {
    let log = Log::new(database_path)?;
    let messages = log.messages().await?;
    dbg!(&messages);
    reply_log(messages, &docker, image_registry.clone()).await;
    let connman = ImplConnMan::new(docker, log, image_registry)?;
    info!("Starting gRPC server on: {}", addr);
    Server::builder()
        .add_service(ConnManServer::new(connman))
        .serve(addr)
        .await?;
    Ok(())
}
