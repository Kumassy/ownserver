use bytes::BytesMut;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use ownserver_lib::{ControlPacketV2, ControlPacketV2Codec, EndpointId, RemoteInfo, StreamId};
use tokio_util::codec::{Decoder, Encoder};

fn bench_encode_init(c: &mut Criterion) {
    let stream_id = StreamId::default();
    let endpoint_id = EndpointId::default();
    let remote_info = RemoteInfo::new("127.0.0.1:8080".parse().unwrap());
    let packet = ControlPacketV2::Init(stream_id, endpoint_id, remote_info);

    c.bench_function("control_packet_encode_init", |b| {
        b.iter(|| {
            let mut codec = ControlPacketV2Codec::new();
            let mut dst = BytesMut::new();
            codec.encode(packet.clone(), &mut dst).unwrap();
            black_box(dst);
        })
    });
}

fn bench_encode_data(c: &mut Criterion) {
    let stream_id = StreamId::default();
    let packet = ControlPacketV2::Data(stream_id, vec![0; 1024]);

    c.bench_function("control_packet_encode_data", |b| {
        b.iter(|| {
            let mut codec = ControlPacketV2Codec::new();
            let mut dst = BytesMut::new();
            codec.encode(packet.clone(), &mut dst).unwrap();
            black_box(dst);
        })
    });
}

fn bench_decode_init(c: &mut Criterion) {
    let stream_id = StreamId::default();
    let endpoint_id = EndpointId::default();
    let remote_info = RemoteInfo::new("127.0.0.1:8080".parse().unwrap());
    let packet = ControlPacketV2::Init(stream_id, endpoint_id, remote_info);

    let mut codec = ControlPacketV2Codec::new();
    let mut encoded = BytesMut::new();
    codec.encode(packet, &mut encoded).unwrap();

    c.bench_function("control_packet_decode_init", |b| {
        b.iter(|| {
            let mut data = encoded.clone();
            let mut codec = ControlPacketV2Codec::new();
            let _ = codec.decode(&mut data).unwrap().unwrap();
            black_box(data);
        })
    });
}

fn bench_decode_data(c: &mut Criterion) {
    let stream_id = StreamId::default();
    let packet = ControlPacketV2::Data(stream_id, vec![0; 1024]);

    let mut codec = ControlPacketV2Codec::new();
    let mut encoded = BytesMut::new();
    codec.encode(packet, &mut encoded).unwrap();

    c.bench_function("control_packet_decode_data", |b| {
        b.iter(|| {
            let mut data = encoded.clone();
            let mut codec = ControlPacketV2Codec::new();
            let _ = codec.decode(&mut data).unwrap().unwrap();
            black_box(data);
        })
    });
}

criterion_group!(benches, bench_encode_init, bench_decode_init, bench_encode_data, bench_decode_data);
criterion_main!(benches);

