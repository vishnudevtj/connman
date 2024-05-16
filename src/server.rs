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
use crate::docker::{Env, ImageId, ImageOption};

use self::grpc::{register_image_response, RegisterImageRequest, RegisterImageResponse};

pub struct ImplConnMan {
    host_port: PortRange,
    connman: Sender<connman::Msg>,
}

impl ImplConnMan {
    pub fn new(connman: Sender<connman::Msg>) -> Self {
        let host_port = PortRange::new(10_000, 30_000);
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

    async fn register_image(
        &self,
        request: Request<RegisterImageRequest>,
    ) -> Result<Response<RegisterImageResponse>, Status> {
        let request: RegisterImageRequest = request.into_inner();
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
                    response: Some(register_image_response::Response::Id(id.into_inner())),
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
) -> anyhow::Result<Proxy> {
    let image_id = ImageId::from(request.id);
    let env = if let (Some(key), Some(value)) = (request.env_key, request.env_value) {
        Some(Env { key, value })
    } else {
        None
    };

    let channel = oneshot::channel();
    let tcp_proxy = TcpPorxy {
        // TODO: ??
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

pub async fn start_grpc(addr: SocketAddr, docker: Sender<connman::Msg>) -> anyhow::Result<()> {
    let connman = ImplConnMan::new(docker);
    info!("Starting gRPC server on: {}", addr);
    Server::builder()
        .add_service(ConnManServer::new(connman))
        .serve(addr)
        .await?;
    Ok(())
}
