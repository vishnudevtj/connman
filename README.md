# connman

This project provides a dynamic way of running containers, starting them only when a user initiates a TCP connection and automatically stopping containers after a specified idle period.


## Requirements

```sh
sudo apt install -y protobuf-compiler libprotobuf-dev build-essential libsqlite3-dev
```

## Configure Docker

Install docker and expose a the REST API which will be used by connman to deploy challenges.
```sh
# https://docs.docker.com/engine/install/ubuntu/
curl -fsSL https://get.docker.com -o get-docker.sh
sudo sh ./get-docker.sh 
```

Inorder to expose REST API of docker run `systemctl edit docker` and add the following line to overwrride the start command of the service. Then restart the service.
Now docker will be listening on host 127.0.0.1 and port 6969 for REST API requests

```
[Service]
ExecStart=
ExecStart=/usr/bin/dockerd -H fd:// -H tcp://127.0.0.1:6969 --containerd=/run/containerd/containerd.sock
```



## Testing the gRPC API

```sh
# Add image
grpcurl   -d '{"image": "aswinmguptha/flashy_machine", "tag": "latest", "port": 5000}'   -proto proto/connman.proto   -plaintext   localhost:50051   connman.ConnMan.RegisterImage
# Add a proxy
grpcurl -d '{"id": "6387861277925541461", "envKey": "VALUE", "envValue": "falg{6387861277925541461}"}' -proto proto/connman.proto -plaintext localhost:50051 connman.ConnMan.AddProxy
```
