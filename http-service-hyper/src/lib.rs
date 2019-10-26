//! `HttpService` server that uses Hyper as backend.

#![warn(
    future_incompatible,
    rust_2018_idioms,
    missing_debug_implementations,
    nonstandard_style,
    missing_docs,
    missing_doc_code_examples
)]

use futures::{future::BoxFuture, prelude::*, task::Spawn};
use futures_tokio_compat::Compat;
use http_service::{Body, HttpService};
use hyper::server::{Builder as HyperBuilder, Server as HyperServer};
#[cfg(feature = "runtime")]
use std::net::SocketAddr;
use std::{
    pin::Pin,
    sync::Arc,
    task::{self, Poll},
};

// Wrapper type to allow us to provide a blanket `MakeService` impl
struct WrapHttpService<H> {
    service: Arc<H>,
}

// Wrapper type to allow us to provide a blanket `Service` impl
struct WrapConnection<H: HttpService> {
    service: Arc<H>,
    connection: H::Connection,
}

fn error_other(error: impl std::error::Error + Send + Sync + 'static) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, error)
}

impl<'a, H, Conn> tower_service::Service<&'a Conn> for WrapHttpService<H>
where
    H: HttpService,
    <H::ConnectionFuture as TryFuture>::Error: std::error::Error + Send + Sync + 'static,
{
    type Response = WrapConnection<H>;
    type Error = std::io::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: &'a Conn) -> Self::Future {
        let service = self.service.clone();
        let connection = service.connect().into_future().map_err(error_other);
        Box::pin(async move {
            let connection = connection.await?;
            Ok(WrapConnection {
                service,
                connection,
            })
        })
    }
}

impl<H> tower_service::Service<hyper::Request<hyper::Body>> for WrapConnection<H>
where
    H: HttpService,
    <H::ResponseFuture as TryFuture>::Error: std::error::Error + Send + Sync + 'static,
{
    type Response = hyper::Response<hyper::Body>;
    type Error = std::io::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: hyper::Request<hyper::Body>) -> Self::Future {
        let req = req.map(|body| {
            Body::from_stream(
                body.map(|res| res.map(|chunk| chunk.into_bytes()).map_err(error_other)),
            )
        });
        let res = self
            .service
            .respond(&mut self.connection, req)
            .into_future()
            .map_err(error_other);

        Box::pin(async move { Ok(res.await?.map(hyper::Body::wrap_stream)) })
    }
}

struct WrapIncoming<I> {
    incoming: I,
}

impl<I: TryStream> hyper::server::accept::Accept for WrapIncoming<I> {
    type Conn = Compat<I::Ok>;
    type Error = I::Error;

    fn poll_accept(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<Self::Conn, Self::Error>>> {
        unsafe { Pin::new_unchecked(&mut self.get_unchecked_mut().incoming) }
            .try_poll_next(cx)
            .map(|opt| opt.map(|res| res.map(Compat::new)))
    }
}

/// A listening HTTP server that accepts connections in both HTTP1 and HTTP2 by default.
///
/// [`Server`] is a [`Future`] mapping a bound listener with a set of service handlers. It is built
/// using the [`Builder`], and the future completes when the server has been shutdown. It should be
/// run by an executor.
#[allow(clippy::type_complexity)] // single-use type with many compat layers
pub struct Server<I: TryStream, S, Sp> {
    inner: HyperServer<WrapIncoming<I>, WrapHttpService<S>, Compat<Sp>>,
}

impl<I: TryStream, S, Sp> std::fmt::Debug for Server<I, S, Sp> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").finish()
    }
}

/// A builder for a [`Server`].
#[allow(clippy::type_complexity)] // single-use type with many compat layers
pub struct Builder<I: TryStream, Sp> {
    inner: HyperBuilder<WrapIncoming<I>, Compat<Sp>>,
}

impl<I: TryStream, Sp> std::fmt::Debug for Builder<I, Sp> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builder").finish()
    }
}

impl<I: TryStream> Server<I, (), ()> {
    /// Starts a [`Builder`] with the provided incoming stream.
    pub fn builder(incoming: I) -> Builder<I, ()> {
        Builder {
            inner: HyperServer::builder(WrapIncoming { incoming }).executor(Compat::new(())),
        }
    }
}

impl<I: TryStream, Sp> Builder<I, Sp> {
    /// Sets the [`Spawn`] to deal with starting connection tasks.
    pub fn with_spawner<Sp2>(self, new_spawner: Sp2) -> Builder<I, Sp2> {
        Builder {
            inner: self.inner.executor(Compat::new(new_spawner)),
        }
    }

    /// Consume this [`Builder`], creating a [`Server`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// #![feature(never_type)]
    ///
    /// use http_service::{Response, Body};
    /// use http_service_hyper::Server;
    /// use romio::TcpListener;
    ///
    /// // Construct an executor to run our tasks on
    /// let mut pool = futures::executor::ThreadPool::new()?;
    ///
    /// // And an HttpService to handle each connection...
    /// let service = |req| {
    ///     futures::future::ok::<_, !>(Response::new(Body::from("Hello World")))
    /// };
    ///
    /// // Then bind, configure the spawner to our pool, and serve...
    /// let addr = "127.0.0.1:3000".parse()?;
    /// let mut listener = TcpListener::bind(&addr)?;
    /// let server = Server::builder(listener.incoming())
    ///     .with_spawner(pool.clone())
    ///     .serve(service);
    ///
    /// // Finally, spawn `server` onto our executor...
    /// pool.run(server)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    pub fn serve<S: HttpService>(self, service: S) -> Server<I, S, Sp>
    where
        I: TryStream + Unpin,
        I::Ok: AsyncRead + AsyncWrite + Send + Unpin + 'static,
        I::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
        Sp: Clone + Spawn + Unpin + Send + 'static,
        <S::ConnectionFuture as TryFuture>::Error: std::error::Error + Send + Sync + 'static,
        <S::ResponseFuture as TryFuture>::Error: std::error::Error + Send + Sync + 'static,
    {
        Server {
            inner: self.inner.serve(WrapHttpService {
                service: Arc::new(service),
            }),
        }
    }
}

impl<I, S, Sp> Future for Server<I, S, Sp>
where
    I: TryStream + Unpin,
    I::Ok: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    I::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S: HttpService,
    Sp: Clone + Spawn + Unpin + Send + 'static,
    <S::ConnectionFuture as TryFuture>::Error: std::error::Error + Send + Sync + 'static,
    <S::ResponseFuture as TryFuture>::Error: std::error::Error + Send + Sync + 'static,
{
    type Output = hyper::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<hyper::Result<()>> {
        self.inner.poll_unpin(cx)
    }
}

/// Serve the given `HttpService` at the given address, using `hyper` as backend, and return a
/// `Future` that can be `await`ed on.
#[cfg(feature = "runtime")]
pub fn serve<S: HttpService>(
    s: S,
    addr: SocketAddr,
) -> impl Future<Output = Result<(), hyper::Error>>
where
    <S::ConnectionFuture as TryFuture>::Error: std::error::Error + Send + Sync + 'static,
    <S::ResponseFuture as TryFuture>::Error: std::error::Error + Send + Sync + 'static,
{
    let service = WrapHttpService {
        service: Arc::new(s),
    };
    hyper::Server::bind(&addr).serve(service)
}
