use rand::prelude::*;
use rand::Rng;
use std::collections::HashSet;
use std::iter::ExactSizeIterator;
use std::ops::RangeBounds;

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

pub struct PortAllocator<R> {
    available_ports: HashSet<u16>,
    range: R,
}

impl<R> PortAllocator<R>
where
    R: RangeBounds<u16> + Clone + Iterator<Item = u16> + ExactSizeIterator,
{
    pub fn new(range: R) -> Self {
        let mut set = HashSet::with_capacity(range.len());
        for p in range.clone().into_iter() {
            set.insert(p);
        }

        PortAllocator {
            available_ports: set,
            range,
        }
    }

    pub fn allocate_port(&mut self, rng: &mut impl Rng) -> Result<u16, PortAllocatorError> {
        if let Some(n) = self.available_ports.iter().choose(rng).map(|n| n.clone()) {
            self.available_ports.remove(&n);
            Ok(n)
        } else {
            Err(PortAllocatorError::AllocationFailed)
        }
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
        let mut alloc = PortAllocator::new(1000..=2000);
        let port = alloc.allocate_port(&mut rng).unwrap();
        assert!(1000 <= port && port <= 2000);
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
        let mut alloc = PortAllocator::new(1000..=2000);
        let port = alloc.release_port(5000);
        assert_eq!(port.err().unwrap(), PortAllocatorError::PortOutOfRange);
    }

    #[test]
    fn return_error_when_port_not_allocated() {
        let mut alloc = PortAllocator::new(1000..=2000);
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

        let _ = alloc.release_port(port).unwrap();
        assert_eq!(alloc.available_ports.len(), 1000);
    }
}
