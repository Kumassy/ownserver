# OwnServer
<p align="center">
  <img alt="overview" src="docs/img/logo.svg" width=100 height=100>
</p>

Expose your local game server to the Internet.

This software aims to minimize cost and effort to prepare local game server like Minecraft, Factorio, RUST and so on.

- Cost you'll save
  - You can utilize any redundunt compute resource for game server as long as they can connect *to* the Internet.
  - You can save cost for Cloud, VPS.
- Effort you'll save
  - No firewall, NAT settings is required.
  - [GUI application](https://github.com/Kumassy/ownserver-client-gui) is also available.

## Features
- Expose your local TCP/UDP endpoint to the Internet
- Offer GUI client for game server for Minecraft, factorio, RUST, etc.

## Installation
Download binary for your OS: 

or build it by yourself:

```
cargo build --release
```

## Usage
:warning: This software has not yet reach stable release. Use with caution! :warning:

We also offer GUI. visit [ownserver-client-gui](https://github.com/Kumassy/ownserver-client-gui)

```
% ./ownserver -h
ownserver 0.5.0

USAGE:
    ownserver [OPTIONS]

FLAGS:
    -h, --help       Prints help information
    -V, --version    Prints version information

OPTIONS:
        --control-port <control-port>    Advanced settings [default: 5000]
        --local-port <local-port>        Port of your local game server listens e.g.) 25565 for Minecraft [default:
                                         3000]
        --payload <payload>              tcp or udp [default: tcp]
        --token-server <token-server>    Advanced settings [default: https://auth.ownserver.kumassy.com/v1/request_token]

# listen on local port
% nc -kl 3000

% ./ownserver --payload tcp --local-port 3000
Connecting to auth server: https://auth.ownserver.kumassy.com/v1/request_token
Your proxy server: shard-7924.ownserver.kumassy.com
Connecting to proxy server: shard-7924.ownserver.kumassy.com:5000
Your Client ID: client_755d0b36-f863-41e1-b5ff-c6c89fdb92a5
+---------------------------------------------------------------------------------------------------+
| Your server tcp://localhost:3000 is now available at tcp://shard-7924.ownserver.kumassy.com:17974 |
+---------------------------------------------------------------------------------------------------+

# you can send any TCP packet to local port!
% nc shard-7924.ownserver.kumassy.com 17974
hello
```

via cargo
```
% cargo run --release --bin ownserver -- -h
```

### Run Minecraft Server
```
# run minecraft server
java -Xmx1024M -Xms1024M -jar server.jar nogui

# run ownserver client
./ownserver  -- --payload tcp --local-port 25565
```

share your public URL!

## How it works
![](/docs/img/overview.svg)

This app creates a private tunnel between our server and your local game server. You'll get a dedicated global public address for your server. 
All requests to that public address are forwarded to your local game server throught the tunnel.


## Similer Project
- [ngrok](https://github.com/inconshreveable/ngrok)
  - written in Go
  - support HTTP, HTTPS and TCP
- [tunnelto](https://github.com/agrinman/tunnelto)
  - written in Rust
  - support HTTP

This software was initially developed as a fork of [tunnelto](https://github.com/agrinman/tunnelto).

## Contributing
### Project Layout
- ownserver/ownserver
  - include executable of client application
  - also serves library for ownserver-client-gui
- ownserver/ownserver_lib
  - define transmission protocol between client and server
- ownserver/ownserver_server
  - establish private tunnel to client
  - forward request between public endpoints to a set of client
- [ownserver-auth](https://github.com/Kumassy/ownserver-auth)
  - performs user authentication and load balancing
- [ownserver-client-gui](https://github.com/Kumassy/ownserver-client-gui)
  - GUI for ownserver client

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