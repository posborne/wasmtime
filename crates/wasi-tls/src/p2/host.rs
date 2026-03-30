use anyhow::Result;
use wasmtime::component::{FixedHostHeapUsage, HostHeapUsage, Resource};
use wasmtime_wasi::async_trait;
use wasmtime_wasi::p2::Pollable;
use wasmtime_wasi::p2::{DynInputStream, DynOutputStream, DynPollable, IoError};

use crate::p2::{
    bindings,
    io::{
        AsyncReadStream, AsyncWriteStream, FutureOutput, WasiFuture, WasiStreamReader,
        WasiStreamWriter,
    },
};
use crate::{TlsStream, TlsTransport, WasiTlsCtxView};

impl<'a> bindings::types::Host for WasiTlsCtxView<'a> {}

/// Represents the ClientHandshake which will be used to configure the handshake
pub struct HostClientHandshake {
    server_name: String,
    transport: Box<dyn TlsTransport>,
}

impl<'a> bindings::types::HostClientHandshake for WasiTlsCtxView<'a> {
    fn new(
        &mut self,
        server_name: String,
        input: Resource<DynInputStream>,
        output: Resource<DynOutputStream>,
    ) -> wasmtime::Result<Resource<HostClientHandshake>> {
        let input = self.table.delete(input)?;
        let output = self.table.delete(output)?;

        let reader = WasiStreamReader::new(input);
        let writer = WasiStreamWriter::new(output);
        let transport = tokio::io::join(reader, writer);

        Ok(self.table.push(HostClientHandshake {
            server_name,
            transport: Box::new(transport) as Box<dyn TlsTransport>,
        })?)
    }

    fn finish(
        &mut self,
        this: Resource<HostClientHandshake>,
    ) -> wasmtime::Result<Resource<HostFutureClientStreams>> {
        let handshake = self.table.delete(this)?;

        let connect = self
            .ctx
            .provider
            .connect(handshake.server_name, handshake.transport);

        let future = HostFutureClientStreams(WasiFuture::spawn(async move {
            let tls_stream = connect.await?;

            let (rx, tx) = tokio::io::split(tls_stream);
            let write_stream = AsyncWriteStream::new(tx);
            let client = HostClientConnection(write_stream.clone());

            let input = Box::new(AsyncReadStream::new(rx)) as DynInputStream;
            let output = Box::new(write_stream) as DynOutputStream;

            Ok((client, input, output))
        }));

        Ok(self.table.push(future)?)
    }

    fn drop(&mut self, this: Resource<HostClientHandshake>) -> wasmtime::Result<()> {
        self.table.delete(this)?;
        Ok(())
    }
}

/// Future streams provides the tls streams after the handshake is completed
pub struct HostFutureClientStreams(
    WasiFuture<Result<(HostClientConnection, DynInputStream, DynOutputStream), IoError>>,
);

#[async_trait]
impl Pollable for HostFutureClientStreams {
    async fn ready(&mut self) {
        self.0.ready().await
    }
}

impl<'a> bindings::types::HostFutureClientStreams for WasiTlsCtxView<'a> {
    fn subscribe(
        &mut self,
        this: Resource<HostFutureClientStreams>,
    ) -> wasmtime::Result<Resource<DynPollable>> {
        wasmtime_wasi::p2::subscribe(self.table, this)
    }

    fn get(
        &mut self,
        this: Resource<HostFutureClientStreams>,
    ) -> wasmtime::Result<
        Option<
            Result<
                Result<
                    (
                        Resource<HostClientConnection>,
                        Resource<DynInputStream>,
                        Resource<DynOutputStream>,
                    ),
                    Resource<IoError>,
                >,
                (),
            >,
        >,
    > {
        let output = self
            .table
            .get_any_mut(this.rep())?
            .downcast_mut::<HostFutureClientStreams>()
            .ok_or(wasmtime::component::ResourceTableError::WrongType)?
            .0
            .get();
        // Drop the borrow before calling self.table.push() below.

        let result = match output {
            FutureOutput::Ready(Ok((client, input, output))) => {
                let client = self.table.push(client)?;
                let input = self.table.push_child(input, &client)?;
                let output = self.table.push_child(output, &client)?;

                Some(Ok(Ok((client, input, output))))
            }
            FutureOutput::Ready(Err(io_error)) => {
                let io_error = self.table.push(io_error)?;

                Some(Ok(Err(io_error)))
            }
            FutureOutput::Consumed => Some(Err(())),
            FutureOutput::Pending => None,
        };

        Ok(result)
    }

    fn drop(&mut self, this: Resource<HostFutureClientStreams>) -> wasmtime::Result<()> {
        self.table.delete(this)?;
        Ok(())
    }
}

/// Represents the client connection and used to shut down the tls stream
pub struct HostClientConnection(
    crate::p2::io::AsyncWriteStream<tokio::io::WriteHalf<Box<dyn TlsStream>>>,
);

impl<'a> bindings::types::HostClientConnection for WasiTlsCtxView<'a> {
    fn close_output(&mut self, this: Resource<HostClientConnection>) -> wasmtime::Result<()> {
        self.table.get_mut(&this)?.0.close()
    }

    fn drop(&mut self, this: Resource<HostClientConnection>) -> wasmtime::Result<()> {
        self.table.delete(this)?;
        Ok(())
    }
}

impl HostHeapUsage for HostClientHandshake {
    fn host_heap_usage(&self) -> usize {
        // TODO: server_name String capacity and the size of the boxed TlsTransport
        // implementation are not tracked beyond the inline struct.
        core::mem::size_of_val(self) + self.server_name.capacity()
    }
}

// HostFutureClientStreams contains a WasiFuture whose internal state
// transitions between Pending(Box<dyn Future>), Ready(...), and Consumed,
// each owning different amounts of heap.
impl HostHeapUsage for HostFutureClientStreams {
    fn host_heap_usage(&self) -> usize {
        // TODO: inspect the WasiFuture state to report the actual heap owned
        // by the pending future or ready values.
        core::mem::size_of_val(self)
    }
}

// HostClientConnection wraps Arc<Mutex<WriteState<IO>>>; close() transitions
// the state machine but does not change the inline struct size.
// TODO: delegates to the inner AsyncWriteStream; see that type's impl.
impl FixedHostHeapUsage for HostClientConnection {}
