# OwnServer
<p align="center">
  <img alt="overview" src="docs/img/logo.svg" width=100 height=100>
</p>

Expose your local game server to the Internet.

This software aims to minimize cost and effort to prepare local game server like Minecraft, Factorio, RUST and so on.

- Cost you'll save
  - You can utilize any redundunt compute resource for game server as long as they can connect to the Internet.
  - You can save cost for Cloud, VPS.
- Effort you'll save
  - No firewall, NAT settings is required.
  - GUI application is also available.

## Features
- Support TCP/UDP

## Installation
Download binary for your OS: 

or build it by yourself:

```
cargo build --release
```

## Usage
for GUI, visit ownserver-client-gui


## How it works
![](/docs/img/overview.svg)

This app creates a private tunnel between our server and your local game server. You'll get a dedicated global public address for your server. 
All requests to that public address are forwarded to your local game server throught the tunnel.


## Similer Project
- ngrok
  - written in Go
  - support HTTP, HTTPS and TCP
- tunnelto
  - written in Rust
  - support HTTP

This software was initially developed as a fork of tunnelto.

## Contributing
### Project Layout
- ownserver/proxy_lib
- ownserver/proxy_client
- ownserver/proxy_server
- ownserver-auth
- ownserver-client-gui

## Running tests

```
cargo test
```


### Self-hosting
TODO

### Issue/PR
Feel free to open Issues, send Pull Requests!


### License
MIT