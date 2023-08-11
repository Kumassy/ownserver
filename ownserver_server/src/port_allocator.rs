use ownserver_lib::Endpoint;
use ownserver_lib::EndpointClaim;
use ownserver_lib::EndpointClaims;
use ownserver_lib::EndpointId;
use ownserver_lib::Endpoints;
use rand::prelude::*;
use rand::Rng;
use std::collections::HashSet;
use std::iter::ExactSizeIterator;
use std::ops::Range;

use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum PortAllocatorError {
    #[error("Port allocation failed because there is no available port.")]
    AllocationFailed,

    #[error("Try to release port that is out of range.")]
    PortOutOfRange,

    #[error("Try to release port that is already exist in available port table.")]
    PortAlreadyReleased,
}

#[derive(Debug)]
pub struct PortAllocator {
    available_ports: HashSet<u16>,
    range: Range<u16>,
}

impl Default for PortAllocator {
    fn default() -> Self {
        PortAllocator::new(10000..20000)
    }
}

impl PortAllocator {
    pub fn new(range: Range<u16>) -> Self {
        let mut set = HashSet::with_capacity(range.len());
        for p in range.clone() {
            set.insert(p);
        }

        PortAllocator {
            available_ports: set,
            range,
        }
    }

    pub fn allocate_port(&mut self, rng: &mut impl Rng) -> Result<u16, PortAllocatorError> {
        if let Some(n) = self.available_ports.iter().choose(rng).copied() {
            self.available_ports.remove(&n);
            Ok(n)
        } else {
            Err(PortAllocatorError::AllocationFailed)
        }
    }

    fn validate_endpoint_claims(&self, claims: &EndpointClaims) -> Result<(), PortAllocatorError> {
        if claims.len() > self.available_ports.len() {
            return Err(PortAllocatorError::AllocationFailed);
        }

        // check local port is unique
        let mut local_ports = HashSet::with_capacity(claims.len());
        for EndpointClaim { local_port, .. } in claims.iter() {
            if !local_ports.insert(*local_port) {
                return Err(PortAllocatorError::AllocationFailed);
            }
        }
        // check remote port is always 0
        for EndpointClaim { remote_port, .. } in claims.iter() {
            if *remote_port != 0 {
                return Err(PortAllocatorError::AllocationFailed);
            }
        }

        Ok(())
    }

    pub fn allocate_ports(&mut self, rng: &mut impl Rng, client_claims: EndpointClaims) -> Result<Endpoints, PortAllocatorError> {
        self.validate_endpoint_claims(&client_claims)?;

        let num_ports = client_claims.len();
        let mut ports = Vec::with_capacity(num_ports);
        for _ in 0..num_ports {
            if let Some(n) = self.available_ports.iter().choose(rng).copied() {
                self.available_ports.remove(&n);
                ports.push(n);
            } else {
                // should never happen
                // return temporary allocated ports back to available_ports
                for p in ports {
                    self.available_ports.insert(p);
                }
                return Err(PortAllocatorError::AllocationFailed);
            }
        }

        let endpoints = client_claims.into_iter().zip(ports.into_iter()).map(|(claim, port)| {
            Endpoint {
                id: EndpointId::new(),
                protocol: claim.protocol,
                local_port: claim.local_port,
                remote_port: port,
            }
        }).collect();

        Ok(endpoints)
    }

    pub fn release_port(&mut self, port: u16) -> Result<(), PortAllocatorError> {
        if !self.range.contains(&port) {
            return Err(PortAllocatorError::PortOutOfRange);
        }
        if self.available_ports.contains(&port) {
            return Err(PortAllocatorError::PortAlreadyReleased);
        }

        self.available_ports.insert(port);

        Ok(())
    }
}

#[cfg(test)]
mod allocate_port_tests {
    use super::*;
    use rand::thread_rng;

    #[test]
    fn allocate_port_at_random() {
        let mut rng = thread_rng();
        let mut alloc = PortAllocator::new(1000..2000);
        let port = alloc.allocate_port(&mut rng).unwrap();
        assert!((1000..2000).contains(&port));
    }

    #[test]
    fn delete_port_from_table() {
        let mut rng = thread_rng();
        let mut alloc = PortAllocator::new(1000..2000);
        assert_eq!(alloc.available_ports.len(), 1000);
        let _ = alloc.allocate_port(&mut rng).unwrap();
        assert_eq!(alloc.available_ports.len(), 999);
    }

    #[test]
    fn return_error_when_no_available_port() {
        let mut rng = thread_rng();
        let mut alloc = PortAllocator::new(1000..1010);
        assert_eq!(alloc.available_ports.len(), 10);

        for _ in 0..10 {
            let _ = alloc.allocate_port(&mut rng).unwrap();
        }
        assert_eq!(alloc.available_ports.len(), 0);

        let port = alloc.allocate_port(&mut rng);
        assert_eq!(port.err().unwrap(), PortAllocatorError::AllocationFailed);
    }
}

#[cfg(test)]
mod release_port_tests {
    use super::*;
    use rand::thread_rng;

    #[test]
    fn return_error_when_port_out_of_range() {
        let mut alloc = PortAllocator::new(1000..2000);
        let port = alloc.release_port(5000);
        assert_eq!(port.err().unwrap(), PortAllocatorError::PortOutOfRange);
    }

    #[test]
    fn return_error_when_port_not_allocated() {
        let mut alloc = PortAllocator::new(1000..2000);
        let port = alloc.release_port(1010);
        assert_eq!(port.err().unwrap(), PortAllocatorError::PortAlreadyReleased);
    }

    #[test]
    fn release_port_to_port_table() {
        let mut rng = thread_rng();
        let mut alloc = PortAllocator::new(1000..2000);
        assert_eq!(alloc.available_ports.len(), 1000);

        let port = alloc.allocate_port(&mut rng).unwrap();
        assert_eq!(alloc.available_ports.len(), 999);

        alloc.release_port(port).unwrap();
        assert_eq!(alloc.available_ports.len(), 1000);
    }
}

#[cfg(test)]
mod validate_endpoint_claims_tests {
    use super::*;
    use ownserver_lib::Protocol;
    
    #[test]
    fn return_error_when_local_port_is_not_unique() {
        let alloc = PortAllocator::new(1000..2000);
        let claims = vec![
            EndpointClaim { protocol: Protocol::TCP, local_port: 1000, remote_port: 0 },
            EndpointClaim { protocol: Protocol::TCP, local_port: 1000, remote_port: 0 },
        ];
        let result = alloc.validate_endpoint_claims(&claims);
        assert_eq!(result.err().unwrap(), PortAllocatorError::AllocationFailed);
    }

    #[test]
    fn return_error_when_remote_port_is_not_zero() {
        let alloc = PortAllocator::new(1000..2000);
        let claims = vec![
            EndpointClaim { protocol: Protocol::TCP, local_port: 1000, remote_port: 0 },
            EndpointClaim { protocol: Protocol::TCP, local_port: 1001, remote_port: 1 },
        ];
        let result = alloc.validate_endpoint_claims(&claims);
        assert_eq!(result.err().unwrap(), PortAllocatorError::AllocationFailed);
    }

    #[test]
    fn return_error_when_ports_are_out_of_stock() {
        let alloc = PortAllocator::new(1000..1001);
        let claims = vec![
            EndpointClaim { protocol: Protocol::TCP, local_port: 1000, remote_port: 0 },
            EndpointClaim { protocol: Protocol::TCP, local_port: 1001, remote_port: 0 },
        ];
        let result = alloc.validate_endpoint_claims(&claims);
        assert_eq!(result.err().unwrap(), PortAllocatorError::AllocationFailed);
    }

    #[test]
    fn return_ok_when_valid() {
        let alloc = PortAllocator::new(1000..2000);
        let claims = vec![
            EndpointClaim { protocol: Protocol::TCP, local_port: 1000, remote_port: 0 },
            EndpointClaim { protocol: Protocol::TCP, local_port: 1001, remote_port: 0 },
        ];
        let result = alloc.validate_endpoint_claims(&claims);
        assert!(result.is_ok());
    }
}

#[cfg(test)]
mod allocate_ports_test {
    use super::*;
    use ownserver_lib::Protocol;

    #[test]
    fn allocate_ports() -> Result<(), PortAllocatorError> {
        let mut rng = thread_rng();
        let mut alloc = PortAllocator::new(1000..2000);

        let claims = vec![
            EndpointClaim { protocol: Protocol::TCP, local_port: 1001, remote_port: 0 },
            EndpointClaim { protocol: Protocol::TCP, local_port: 1002, remote_port: 0 },
        ];

        let endpoints = alloc.allocate_ports(&mut rng, claims)?;

        assert_eq!(endpoints.len(), 2);
        assert_eq!(endpoints[0].local_port, 1001);
        assert!((1000..2000).contains(&endpoints[0].remote_port));
        assert_eq!(endpoints[1].local_port, 1002);
        assert!((1000..2000).contains(&endpoints[1].remote_port));

        Ok(())
    }

    #[test]
    fn delete_ports_from_table() -> Result<(), PortAllocatorError> {
        let mut rng = thread_rng();
        let mut alloc = PortAllocator::new(1000..2000);

        let claims = vec![
            EndpointClaim { protocol: Protocol::TCP, local_port: 1001, remote_port: 0 },
            EndpointClaim { protocol: Protocol::TCP, local_port: 1002, remote_port: 0 },
        ];

        assert_eq!(alloc.available_ports.len(), 1000);
        let _ = alloc.allocate_ports(&mut rng, claims)?;
        assert_eq!(alloc.available_ports.len(), 998);

        Ok(())
    }

    #[test]
    fn return_error_when_no_available_port() {
        let mut rng = thread_rng();
        let mut alloc = PortAllocator::new(1000..1001);

        let claims = vec![
            EndpointClaim { protocol: Protocol::TCP, local_port: 1001, remote_port: 0 },
            EndpointClaim { protocol: Protocol::TCP, local_port: 1002, remote_port: 0 },
        ];

        let endpoints = alloc.allocate_ports(&mut rng, claims);
        assert_eq!(endpoints.err().unwrap(), PortAllocatorError::AllocationFailed);
    }
}