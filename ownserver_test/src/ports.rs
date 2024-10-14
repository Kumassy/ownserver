use std::{net::TcpListener, ops::Range, sync::{LazyLock, Mutex}};


const PORT_POOL_RANGE: Range<u16> = 5000..6000;
const PORT_POOL_NUM: usize = 100;
const PORT_POOL_BULK_RANGE: Range<u16> = 6000..7000;
const PORT_POOL_BULK_SIZE: u16 = 5;
const PORT_POOL_BULK_NUM: usize = 20;

static AVAILABLE_PORTS: LazyLock<Mutex<Vec<u16>>> = LazyLock::new(|| {
    find_available_ports().into()
});

static AVAILABLE_BULK_PORTS: LazyLock<Mutex<Vec<Range<u16>>>> = LazyLock::new(|| {
    find_available_bulk_ports().into()
});

fn is_available(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn find_available_ports() -> Vec<u16> {
    let mut available_ports = Vec::new();

    let mut port = PORT_POOL_RANGE.start;

    while available_ports.len() < PORT_POOL_NUM {
        if is_available(port) {
            available_ports.push(port)
        }

        port += 1;

        if port >= PORT_POOL_RANGE.end {
            panic!("could not find available ports for PORT_POOL");
        }
    }

    available_ports
}

fn find_available_bulk_ports() -> Vec<Range<u16>> {
    let mut available_ports = Vec::new();

    let mut port = PORT_POOL_BULK_RANGE.start;

    while available_ports.len() < PORT_POOL_BULK_NUM {
        let port_range = Range { start: port, end: port + PORT_POOL_BULK_SIZE};

        let port_vec: Vec<u16> = port_range.clone().collect();
        if port_vec.iter().all(|&p| is_available(p)) {
            available_ports.push(port_range);
        }

        port += PORT_POOL_BULK_SIZE;
    }

    available_ports
}


pub fn use_ports<const N: usize>() -> [u16; N] {
    let mut ports = Vec::new();
    for _ in 0..N {
        if let Some(port) = AVAILABLE_PORTS.lock().unwrap().pop() {
            ports.push(port);
        } else {
            panic!("no available ports in port pool")
        }
    }

    ports.try_into().expect("incorrect number of ports collected")
}

pub fn use_bulk_ports() -> Range<u16> {
    if let Some(ports) = AVAILABLE_BULK_PORTS.lock().unwrap().pop() {
        return ports
    }

    panic!("no available ports in bulk port pool");
}


pub struct PortSet {
    pub token_port: u16,
    pub control_port: u16,
    pub remote_ports: Range<u16>,
}

impl PortSet {
    pub fn new() -> Self {
        let [ token_port, control_port ] = use_ports();
        let remote_ports = use_bulk_ports();

        Self {
            token_port,
            control_port,
            remote_ports,
        }
    }
}

impl Default for PortSet {
    fn default() -> Self {
        Self::new()
    }
}