//! A scriptable USB device for driver tests.
//!
//! Tests describe the scanner as a closure answering each [`Transfer`]; every
//! transfer is logged so the test can assert on the exact bytes a driver sent.
//! A mock only proves a driver matches the test author's reading of the C
//! source. Real recordings ([`crate::Replay`]) are stronger evidence.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::{DeviceIds, Error, Result, UsbTransport};

/// One transfer the driver issued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transfer {
    BulkOut(Vec<u8>),
    BulkIn(usize),
    InterruptIn(usize),
    ControlOut { request_type: u8, request: u8, value: u16, index: u16, data: Vec<u8> },
    ControlIn { request_type: u8, request: u8, value: u16, index: u16, len: usize },
}

/// How the device answers: IN transfers return the bytes, OUT ones ignore them.
pub type Handler = Box<dyn FnMut(&Transfer) -> Result<Vec<u8>> + Send>;

/// The transfers a [`Mock`] saw. Stays readable after the driver owns the mock.
pub type Log = Arc<Mutex<Vec<Transfer>>>;

/// A device simulated by a closure.
pub struct Mock {
    ids: DeviceIds,
    handler: Handler,
    log: Log,
}

impl std::fmt::Debug for Mock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mock").field("ids", &self.ids).finish_non_exhaustive()
    }
}

impl Mock {
    /// Creates a device with these IDs answering through `handler`.
    pub fn new(
        vendor_id: u16,
        product_id: u16,
        handler: impl FnMut(&Transfer) -> Result<Vec<u8>> + Send + 'static,
    ) -> Self {
        Self { ids: DeviceIds { vendor_id, product_id, serial: None }, handler: Box::new(handler), log: Log::default() }
    }

    /// A handle on the transfer log.
    #[must_use]
    pub fn log(&self) -> Log {
        Arc::clone(&self.log)
    }

    fn run(&mut self, t: Transfer) -> Result<Vec<u8>> {
        let r = (self.handler)(&t);
        self.log.lock().unwrap_or_else(PoisonError::into_inner).push(t);
        r
    }

    fn read(&mut self, t: Transfer, buf: &mut [u8]) -> Result<usize> {
        let data = self.run(t)?;
        if data.len() > buf.len() {
            return Err(Error::Io(format!("mock sent {} bytes, driver asked for {}", data.len(), buf.len())));
        }
        buf[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }
}

impl UsbTransport for Mock {
    fn ids(&self) -> &DeviceIds {
        &self.ids
    }

    fn bulk_out(&mut self, data: &[u8], _timeout: Duration) -> Result<usize> {
        self.run(Transfer::BulkOut(data.to_vec()))?;
        Ok(data.len())
    }

    fn bulk_in(&mut self, buf: &mut [u8], _timeout: Duration) -> Result<usize> {
        self.read(Transfer::BulkIn(buf.len()), buf)
    }

    fn interrupt_in(&mut self, buf: &mut [u8], _timeout: Duration) -> Result<usize> {
        self.read(Transfer::InterruptIn(buf.len()), buf)
    }

    fn control_in(
        &mut self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        buf: &mut [u8],
        _timeout: Duration,
    ) -> Result<usize> {
        self.read(Transfer::ControlIn { request_type, request, value, index, len: buf.len() }, buf)
    }

    fn control_out(
        &mut self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &[u8],
        _timeout: Duration,
    ) -> Result<usize> {
        self.run(Transfer::ControlOut { request_type, request, value, index, data: data.to_vec() })?;
        Ok(data.len())
    }

    fn sleep(&mut self, _duration: Duration) {}
}
