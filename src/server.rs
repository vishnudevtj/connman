use std::net::SocketAddr;

use log::info;
use tokio::sync::mpsc::Sender;
use tokio::sync::{oneshot, Mutex};
use tonic::{transport::Server, Request, Response, Status};

pub mod grpc {
    tonic::include_proto!("connman");
}

use grpc::conn_man_server::{ConnMan, ConnManServer};
use grpc::{
    add_proxy_response, AddProxyRequest, AddProxyResponse, AddTlsListenerRequest,
    AddTlsListenerResponse, ChallengeType, Proxy, RemoveProxyRequest, RemoveProxyResponse,
};

use crate::connman::{self, PortRange, TcpPorxy};
use crate::docker::{Env, ImageOption};

pub struct ImplConnMan {
    host_port: Mutex<PortRange>,
    connman: Sender<connman::Msg>,
}

impl ImplConnMan {
    pub fn new(connman: Sender<connman::Msg>) -> Self {
        let host_port = Mutex::new(PortRange::new(20_000, 30_000));
        Self { connman, host_port }
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

    async fn add_proxy(
        &self,
        request: Request<AddProxyRequest>,
    ) -> Result<Response<AddProxyResponse>, Status> {
        let request: AddProxyRequest = request.into_inner();
        // TODO: unwrap
        let host_port = { self.host_port.lock().await.aquire_port().unwrap() };

        let res = _add_proxy(&self.connman, host_port, request)
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
        todo!()
    }
}

pub async fn start_grpc(addr: SocketAddr, docker: Sender<connman::Msg>) -> anyhow::Result<()> {
    let connman = ImplConnMan::new(docker);
    info!("Starting gRPC server on: {}", addr);
    Server::builder()
        .add_service(ConnManServer::new(connman))
        .serve(addr)
        .await?;
    Ok(())
}

async fn _add_proxy(
    connman: &Sender<connman::Msg>,
    host_port: u16,
    request: AddProxyRequest,
) -> anyhow::Result<Proxy> {
    // Register the image.
    let image_option = ImageOption {
        always_pull: true,
        service_port: request.port as u16,
        name: request.image,
        tag: String::from("latest"),
        credentials: None,
    };

    let channel = oneshot::channel();
    let msg = connman::Msg::RegisterImage(image_option, channel.0);
    connman.send(msg).await?;

    let image_id = channel.1.await??;

    let env = if let (Some(key), Some(value)) = (request.env_key, request.env_value) {
        Some(Env { key, value })
    } else {
        None
    };

    let channel = oneshot::channel();
    let tcp_proxy = TcpPorxy {
        host: "0.0.0.0".to_string(),
        listen_port: host_port,
        image: image_id,
        env,
    };

    let msg = connman::Msg::TcpPorxy(tcp_proxy, channel.0);
    connman.send(msg).await?;
    let url = channel.1.await??;

    Ok(Proxy {
        // TODO: implement logic for id.
        proxy_id: 0,
        host: url.host,
        port: url.port.to_string(),
    })
}
