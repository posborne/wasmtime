use crate::sockets::{SocketAddrCheck, SocketAddressFamily};
use std::net::SocketAddr;
use std::sync::Arc;
use wasmtime::component::{FixedHostHeapUsage, HostHeapUsage};

pub struct IncomingDatagramStream {
    pub(crate) inner: Arc<tokio::net::UdpSocket>,

    /// If this has a value, the stream is "connected".
    pub(crate) remote_address: Option<SocketAddr>,
}

pub struct OutgoingDatagramStream {
    pub(crate) inner: Arc<tokio::net::UdpSocket>,

    /// If this has a value, the stream is "connected".
    pub(crate) remote_address: Option<SocketAddr>,

    /// Socket address family.
    pub(crate) family: SocketAddressFamily,

    /// The check of allowed addresses
    pub(crate) socket_addr_check: Option<SocketAddrCheck>,

    /// Remaining number of datagrams permitted by most recent `check-send`
    /// call.
    pub(crate) check_send_permit_count: usize,
}

// Both datagram streams transition between fixed-size enum states (SendState);
// their inline sizes are constant regardless of which state is active.
// IncomingDatagramStream: the Arc<UdpSocket> is shared with the parent and the
// actual socket buffer lives in kernel space.
// OutgoingDatagramStream: TODO - SocketAddrCheck contains an Arc<dyn Fn(...)>
// whose closure allocation is not tracked.
impl FixedHostHeapUsage for IncomingDatagramStream {}
impl FixedHostHeapUsage for OutgoingDatagramStream {}
