TODO
- [x] local to server buffer
- [x] process control flow message
- [x] tunnel_client
- [x] server hello, connect_wormhole
- [x] make ACTIVE_STREAMS to DI
- [x] write test to process_client_messages
- [x] skip tracing, make log clean
- [x] more meaningful log
    - [x] include stream id to each log 
    - [x] num of client, stream at create/delete client, stream
- [x] client: run_wormhole
- [x] add version field to handshake
- [x] host -> port addresssing
    - this is not necessary because port belongs to only one host: remote process
- [x] launch, delete port listen when client registered/dropped
- [x] maintain available port table
- [ ] forbit port scan, integrate with fail2ban
- [ ] add more auth logics
- [ ] add admin api: get available port count for load balancing 
- [ ] monitoring
    - [ ] cpu, # of open tcp port
- [x] no unwrap, expect
- [ ] 2 hour auto delete
    - for rolling update
- [x] add test, which checks api version field
- [x] remove client id from client hello
- [x] add error in server hello
- [x] handle error in server_hello
    

integration test
- [x] if server send ControlPacket::Init, then client set up local stream

stress test
- [ ] HTTP server for client-side server
- [ ] Minecraft headless server for client-side server

(host, client_id) の組は一通りではない...？
https://github.com/agrinman/tunnelto/blob/master/tunnelto_server/src/connected_clients.rs#L49



## Deploy
### Enable Tracing on AWS
> https://aws-otel.github.io/docs/getting-started/x-ray
> https://aws-otel.github.io/docs/setup/ec2
> https://github.com/aws-observability/aws-otel-collector/blob/main/docs/developers/linux-rpm-demo.md

```
wget https://aws-otel-collector.s3.amazonaws.com/amazon_linux/amd64/latest/aws-otel-collector.rpm
wget https://aws-otel-collector.s3.amazonaws.com/aws-otel-collector.gpg
sudo rpm --import aws-otel-collector.gpg
rpm --checksig aws-otel-collector.rpm

sudo /opt/aws/aws-otel-collector/bin/aws-otel-collector-ctl -c  otel-collector.yaml -a start

sudo /opt/aws/aws-otel-collector/bin/aws-otel-collector-ctl -c  otel-collector.yaml -a status
{
  "status": "running",
  "starttime": "2022-04-23T12:48:23+0000",
  "version": "v0.16.1"
}

[ec2-user@ip-10-0-0-8 magic-tunnel]$ sudo lsof -p 17974 -P
COMMAND     PID USER   FD      TYPE             DEVICE SIZE/OFF     NODE NAME
aws-otel- 17974  aoc  cwd       DIR              202,1      257       96 /
aws-otel- 17974  aoc  rtd       DIR              202,1      257       96 /
aws-otel- 17974  aoc  txt       REG              202,1 89711064 25667806 /opt/aws/aws-otel-collector/bin/aws-otel-collector
aws-otel- 17974  aoc    0r      CHR                1,3      0t0        4 /dev/null
aws-otel- 17974  aoc    1u     unix 0xffff88800659d400      0t0 10837929 socket
aws-otel- 17974  aoc    2u     unix 0xffff88800659d400      0t0 10837929 socket
aws-otel- 17974  aoc    3w      REG              202,1     7548   664278 /opt/aws/aws-otel-collector/logs/aws-otel-collector.log
aws-otel- 17974  aoc    4u  a_inode               0,12        0    10885 [eventpoll]
aws-otel- 17974  aoc    5r     FIFO               0,11      0t0 10837933 pipe
aws-otel- 17974  aoc    6w     FIFO               0,11      0t0 10837933 pipe
aws-otel- 17974  aoc    7u     IPv6           10837951      0t0      UDP *:2000
aws-otel- 17974  aoc    8u     IPv6           10837954      0t0      TCP *:13133 (LISTEN)
aws-otel- 17974  aoc    9u     IPv6           10837957      0t0      UDP *:6832
aws-otel- 17974  aoc   10u     IPv6           10837958      0t0      UDP *:6831
aws-otel- 17974  aoc   11u     IPv6           10837959      0t0      TCP *:14268 (LISTEN)
aws-otel- 17974  aoc   12u     IPv6           10837960      0t0      TCP *:14250 (LISTEN)
aws-otel- 17974  aoc   13u     IPv6           10837961      0t0      TCP *:4317 (LISTEN)
aws-otel- 17974  aoc   14u     IPv6           10837962      0t0      TCP *:55681 (LISTEN)
aws-otel- 17974  aoc   15u     IPv6           10837968      0t0      TCP *:2000 (LISTEN)
aws-otel- 17974  aoc   16u     IPv6           10837970      0t0      TCP *:8888 (LISTEN)
```

https://docs.aws.amazon.com/ja_jp/xray/latest/devguide/xray-daemon-ec2.html



```
[ec2-user@ip-10-0-0-8 magic-tunnel]$ cat otel-collector.yaml
extensions:
  health_check:

receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317
      http:
        endpoint: 0.0.0.0:55681
          # awsxray:
          # endpoint: 0.0.0.0:2000
          # transport: udp
  jaeger:
    protocols:
      grpc:
      thrift_binary:
      thrift_compact:
      thrift_http:

processors:
  batch/traces:
    timeout: 1s
    send_batch_size: 50
  batch/metrics:
    timeout: 60s

exporters:
  awsxray:
  awsemf:

service:
  pipelines:
    traces:
      # receivers: [otlp,awsxray,jaeger]
      receivers: [otlp,jaeger]
      processors: [batch/traces]
      exporters: [awsxray]
    metrics:
      receivers: [otlp]
      processors: [batch/metrics]
      exporters: [awsemf]

  extensions: [health_check]
```


### OTelCol 
```
$ sudo apt-get update
$ sudo apt-get -y install wget systemctl
$ wget https://github.com/open-telemetry/opentelemetry-collector-releases/releases/download/v0.44.0/otelcol_0.44.0_linux_arm64.deb
$ dpkg -i otelcol_0.44.0_linux_arm64.deb

$ sudo vim /etc/otelcol/config.yaml
$ sudo cat /etc/otelcol/config.yaml
extensions:
  health_check:
  pprof:
    endpoint: 0.0.0.0:1777
  zpages:
    endpoint: 0.0.0.0:55679

receivers:
  otlp:
    protocols:
      grpc:
      http:

  opencensus:

  # Collect own metrics
  prometheus:
    config:
      scrape_configs:
      - job_name: 'otel-collector'
        scrape_interval: 10s
        static_configs:
        - targets: ['0.0.0.0:8888']

  jaeger:
    protocols:
      grpc:
      thrift_binary:
      thrift_compact:
      thrift_http:

  zipkin:

processors:
  batch:

exporters:
  logging:
    logLevel: debug
  jaeger:
    endpoint: 10.0.0.5:14250
    tls:
      insecure: true

service:

  pipelines:

    traces:
      receivers: [otlp, opencensus, jaeger, zipkin]
      processors: [batch]
      exporters: [jaeger]

      # metrics:
      # receivers: [otlp, opencensus, prometheus]
      # processors: [batch]
      # exporters: [logging]

      # extensions: [health_check, pprof, zpages]

$ sudo systemctl restart otelcol
```