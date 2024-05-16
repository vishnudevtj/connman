# connman

This project provides a dynamic way of running containers, starting them only when a user initiates a TCP connection and automatically stopping containers after a specified idle period.


## Requirements

```sh
sudo apt install -y protobuf-compiler libprotobuf-dev
```

```sh
# Add image
grpcurl   -d '{"image": "aswinmguptha/flashy_machine", "tag": "latest", "port": 5000}'   -proto proto/connman.proto   -plaintext   localhost:50051   connman.ConnMan.RegisterImage
# Add a proxy
grpcurl -d '{"id": "6387861277925541461", "envKey": "VALUE", "envValue": "falg{6387861277925541461}"}' -proto proto/connman.proto -plaintext localhost:50051 connman.ConnMan.AddProxy
```
