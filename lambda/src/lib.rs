#![feature(async_await)]
#![deny(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![warn(missing_docs, nonstandard_style, rust_2018_idioms)]

//! This is the core of the Lambda Runtime.
use bytes::Bytes;
use futures::{
    future::BoxFuture,
    prelude::*,
    task::{Context, Poll},
};
use http::{
    uri::{Authority, Scheme},
    Method, Request, Response, Uri,
};
use serde::{Deserialize, Serialize};
use std::{
    convert::TryFrom,
    env,
    marker::Unpin,
    mem,
    pin::Pin,
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

pub use lambda_attributes::lambda;

/// A module containing various types availible a Lambda function.
pub mod types;
use crate::types::LambdaCtx;

type Err = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Debug)]
/// A string error, which can be display
pub(crate) struct StringError(pub String);

impl std::error::Error for StringError {}

impl std::fmt::Display for StringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        self.0.fmt(f)
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! err_fmt {
    {$($t:tt)*} => {
        $crate::StringError(format!($($t)*))
    }
}

/// A struct containing configuration values derived from environment variables.
#[derive(Debug, Default)]
pub struct Config {
    /// The host and port of the [runtime API](https://docs.aws.amazon.com/lambda/latest/dg/runtimes-api.html).
    pub endpoint: String,
    /// The name of the function.
    pub function_name: String,
    /// The amount of memory available to the function in MB.
    pub memory: i32,
    /// The version of the function being executed.
    pub version: String,
    /// The name of the Amazon CloudWatch Logs stream for the function.
    pub log_stream: String,
    /// The name of the Amazon CloudWatch Logs group for the function.
    pub log_group: String,
}

impl Config {
    /// Attempts to read configuration from environment variables.
    pub fn from_env() -> Result<Self, Err> {
        let conf = Config {
            endpoint: env::var("AWS_LAMBDA_RUNTIME_API")?,
            function_name: env::var("AWS_LAMBDA_FUNCTION_NAME")?,
            memory: env::var("AWS_LAMBDA_FUNCTION_MEMORY_SIZE")?.parse::<i32>()?,
            version: env::var("AWS_LAMBDA_FUNCTION_VERSION")?,
            log_stream: env::var("AWS_LAMBDA_LOG_STREAM_NAME")?,
            log_group: env::var("AWS_LAMBDA_LOG_GROUP_NAME")?,
        };
        Ok(conf)
    }
}

#[derive(Debug)]
struct Client {
    scheme: Scheme,
    authority: Authority,
    client: hyper::Client<hyper::client::HttpConnector>,
}

impl Client {
    fn new(scheme: Scheme, authority: Authority) -> Self {
        Self {
            scheme,
            authority,
            client: hyper::Client::new(),
        }
    }
}

/// A trait modeling interactions with the [Lambda Runtime API](https://docs.aws.amazon.com/lambda/latest/dg/runtimes-api.html).
trait EventClient<'a>: Send + Sync + Unpin {
    /// A future containing the next event from the Lambda Runtime API.
    type Fut: Future<Output = Result<Response<Bytes>, Err>> + Send + 'a;
    fn call(&self, req: Request<Bytes>) -> Self::Fut;
}

impl<'a> EventClient<'a> for Client {
    type Fut = BoxFuture<'a, Result<Response<Bytes>, Err>>;

    fn call(&self, req: Request<Bytes>) -> Self::Fut {
        use futures::compat::{Future01CompatExt, Stream01CompatExt};
        use pin_utils::pin_mut;

        let (mut parts, body) = req.into_parts();
        let pq = parts.uri.path_and_query().unwrap();
        let uri = Uri::builder()
            .scheme(self.scheme.clone())
            .authority(self.authority.clone())
            .path_and_query(pq.clone())
            .build()
            .unwrap();
        parts.uri = uri;
        let body = hyper::Body::from(body);
        let req = Request::from_parts(parts, body);

        let res = self.client.request(req).compat();
        let fut = async {
            let res = res.await?;
            let (parts, body) = res.into_parts();
            let body = body.compat();
            pin_mut!(body);

            let mut buf: Vec<u8> = vec![];
            while let Some(Ok(chunk)) = body.next().await {
                let mut chunk: Vec<u8> = chunk.into_bytes().to_vec();
                buf.append(&mut chunk)
            }
            let buf = Bytes::from(buf);
            let res = Response::from_parts(parts, buf);
            Ok(res)
        };

        fut.boxed()
    }
}

/// The `Stream` implementation for `EventStream` converts a `Future`
/// containing the next event from the Lambda Runtime into a continuous
/// stream of events. While _this_ stream will continue to produce
/// events indefinitely, AWS Lambda will only run the Lambda function attached
/// to this runtime *if and only if* there is an event available for it to process.
/// For Lambda functions that receive a “warm wakeup”—i.e., the function is
/// readily available in the Lambda service's cache—this runtime is able
/// to immediately fetch the next event.
struct EventStream<'a, T>
where
    T: EventClient<'a>,
{
    current: Option<BoxFuture<'a, Result<Response<Bytes>, Err>>>,
    client: &'a T,
}

impl<'a, T> EventStream<'a, T>
where
    T: EventClient<'a>,
{
    fn new(inner: &'a T) -> Self {
        Self {
            current: None,
            client: inner,
        }
    }

    fn next_event(&self) -> BoxFuture<'a, Result<Response<Bytes>, Err>> {
        let req = Request::builder()
            .method(Method::GET)
            .uri(Uri::from_static("/runtime/invocation/next"))
            .body(Bytes::new())
            .unwrap();
        Box::pin(self.client.call(req))
    }
}

#[must_use = "streams do nothing unless you `.await` or poll them"]
impl<'a, T> Stream for EventStream<'a, T>
where
    T: EventClient<'a>,
{
    type Item = Result<Response<Bytes>, Err>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // The `loop` is used to drive the inner future (`current`) to completion, advancing
        // the state of this stream to yield a new `Item`. Loops like the one below are
        // common in many hand-implemented `Futures` and `Streams`.
        loop {
            // The stream first checks an inner future is set. If the future is present,
            // a runtime polls the inner future to completion.
            if let Some(current) = &mut self.current {
                match current.as_mut().poll(cx) {
                    // If the inner future signals readiness, we:
                    // 1. Create a new Future that represents the _next_ event which will be polled
                    // by subsequent iterations of this loop.
                    // 2. Return the current future, yielding the resolved future.
                    Poll::Ready(res) => {
                        let next = self.next_event();
                        self.current = Some(Box::pin(next));
                        return Poll::Ready(Some(res));
                    }
                    // Otherwise, the future signals that it's not ready, so we propagate the
                    // Poll::Pending signal to the caller.
                    Poll::Pending => return Poll::Pending,
                }
            } else {
                self.current = Some(self.next_event());
            }
        }
    }
}
// we mimic how std::alloc provides global hooks for allocation errors.
static HOOK: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// A computer-readable report of an unhandled error.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct ErrorReport {
    /// The type of the error passed to the Lambda APIs.
    pub name: String,
    /// The [std::fmt::Display] output of the error.
    pub err: String,
}

fn default_error_hook(err: Err) -> ErrorReport {
    ErrorReport {
        name: String::from("UnknownError"),
        err: format!("{}", err),
    }
}

// called by the runtime APIs
fn error(err: Err) -> ErrorReport {
    let hook = HOOK.load(Ordering::SeqCst);
    let hook: fn(Err) -> ErrorReport = if hook.is_null() {
        default_error_hook
    } else {
        unsafe { mem::transmute(hook) }
    };
    hook(err)
}

/// Registers a custom error hook, replacing any that was previously registered.
///
/// The Lambda error hook is invoked when a [Handler] or [HttpHandler] returns an error, but prior
/// to the runtime reporting the error to the Lambda Runtime APIs. This hook is intended to be used
/// by those interested in programatialy
///
/// This function, in terms of intended usage and implementation, mimics [std::alloc::set_alloc_error_hook].
pub fn set_error_hook<E: Into<Err>>(hook: fn(err: E) -> ErrorReport) {
    HOOK.store(hook as *mut (), Ordering::SeqCst);
}

#[test]
fn set_err_hook() {
    set_error_hook(|err: Err| {
        if let Some(e) = err.downcast_ref::<std::io::Error>() {
            ErrorReport {
                name: String::from("std::io::Error"),
                err: format!("{}", e),
            }
        } else {
            default_error_hook(err)
        }
    });

    let e = err_fmt!("An error");
    let e = error(e.into());
    assert_eq!(String::from("UnknownError"), e.name)
}

/// Returns a new `HandlerFn` with the given closure.
pub fn handler_fn<F>(f: F) -> HandlerFn<F> {
    HandlerFn { f }
}

/// A `Handler` or `HttpHandler` implemented by a closure.
#[derive(Copy, Clone, Debug)]
pub struct HandlerFn<F> {
    f: F,
}

impl<F, A, B, E, Fut> Handler<A, B> for HandlerFn<F>
where
    F: Fn(A) -> Fut,
    A: for<'de> Deserialize<'de>,
    B: Serialize,
    E: Into<Err>,
    Fut: Future<Output = Result<B, E>> + Send,
{
    type Err = E;
    type Fut = Fut;
    fn call(&mut self, req: A, _: LambdaCtx) -> Self::Fut {
        (self.f)(req)
    }
}

impl<F, A, B, E, Fut> Handler<A, B> for HandlerFn<F>
where
    F: Fn(A, LambdaCtx) -> Fut,
    A: for<'de> Deserialize<'de>,
    B: Serialize,
    E: Into<Err>,
    Fut: Future<Output = Result<B, E>> + Send,
{
    type Err = E;
    type Fut = Fut;
    fn call(&mut self, req: A, ctx: LambdaCtx) -> Self::Fut {
        // we pass along the context here
        (self.f)(req, ctx)
    }
}

impl<F, A, B, E, Fut> HttpHandler<A, B> for HandlerFn<F>
where
    F: Fn(Request<A>) -> Fut,
    A: for<'de> Deserialize<'de>,
    B: Serialize,
    E: Into<Err>,
    Fut: Send + Future<Output = Result<Response<B>, E>>,
{
    type Err = E;
    type Fut = Fut;
    fn call(&mut self, req: Request<A>) -> Self::Fut {
        (self.f)(req)
    }
}

/// A trait describing an asynchronous function from `In` to `Out`. `In` and `Out` must implement [`Deserialize`](serde::Deserialize) and [`Serialize`](serde::Serialize).
pub trait Handler<A, B>
where
    A: for<'de> Deserialize<'de>,
    B: Serialize,
{
    /// Errors returned by this handler.
    type Err: Into<Err>;
    /// The future response value of this handler.
    type Fut: Future<Output = Result<B, Self::Err>>;
    /// Process the incoming event and return the response asynchronously.
    fn call(&mut self, event: A, ctx: LambdaCtx) -> Self::Fut;
}

/// A trait describing an asynchronous function from `Request<A>` to `Response<B>`. `A` and `B` must implement [`Deserialize`](serde::Deserialize) and [`Serialize`](serde::Serialize).
///
/// This trait is a specialized version of [`Handler`]. Its primary use is for Lambda functions
/// which are targets of API Gateway or an Application Load Balancer.
pub trait HttpHandler<A, B>
where
    A: for<'de> Deserialize<'de>,
    B: Serialize,
{
    /// Error returned by this handler.
    type Err: Into<Err>;
    /// The future response value of this handler.
    type Fut: Future<Output = Result<Response<B>, Self::Err>>;
    /// Process the incoming request and return the response asynchronously.
    fn call(&mut self, event: Request<A>) -> Self::Fut;
}

/// Runs an [Handler].
pub async fn run<F, A, B>(
    mut handler: F,
) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>>
where
    F: Handler<A, B>,
    A: for<'de> Deserialize<'de>,
    B: Serialize,
{
    let mut config = Config::default();
    config.endpoint = String::from("http://localhost:8000");
    let parts = config.endpoint.parse::<Uri>()?.into_parts();
    let scheme = parts.scheme.ok_or(err_fmt!("scheme not found"))?;
    let authority = parts.authority.ok_or(err_fmt!("authority not found"))?;

    let client = Client::new(scheme, authority);
    let mut stream = EventStream::new(&client);

    while let Some(event) = stream.next().await {
        let (parts, body) = event?.into_parts();
        let ctx: LambdaCtx = LambdaCtx::try_from(parts.headers)?;
        let body = serde_json::from_slice(&body)?;

        match handler.call(body, ctx).await {
            Ok(res) => {
                let res = serde_json::to_vec(&res)?;
                let uri = format!("/runtime/invocation/{}/response", ctx.id).parse::<Uri>()?;
                let req = Request::builder()
                    .uri(uri)
                    .method(Method::POST)
                    .body(Bytes::from(res))?;

                client.call(req).await?;
            }
            Err(err) => {
                let err = error(err.into());
                let err = serde_json::to_vec(&err)?;
                let uri = format!("/runtime/invocation/{}/error", ctx.id).parse::<Uri>()?;
                let req = Request::builder()
                    .uri(uri)
                    .method(Method::POST)
                    .body(Bytes::from(err))?;

                client.call(req).await?;
            }
        }
    }

    Ok(())
}

/// Runs an [HttpHandler].
pub async fn run_http<F, A, B>(mut handler: F) -> Result<(), Err>
where
    F: HttpHandler<A, B>,
    A: for<'de> Deserialize<'de>,
    B: Serialize,
{
    let mut config = Config::default();
    config.endpoint = String::from("http://localhost:8000");
    let parts = config.endpoint.parse::<Uri>()?.into_parts();
    let scheme = parts.scheme.ok_or(err_fmt!("scheme not found"))?;
    let authority = parts.authority.ok_or(err_fmt!("authority not found"))?;

    let client = Client::new(scheme, authority);
    let mut stream = EventStream::new(&client);

    while let Some(event) = stream.next().await {
        let (parts, body) = event?.into_parts();
        let ctx: LambdaCtx = LambdaCtx::try_from(parts.headers)?;
        let body = serde_json::from_slice(&body)?;
        let req = Request::new(body);

        match handler.call(req).await {
            Ok(res) => {
                let (parts, body) = res.into_parts();
                let res = serde_json::to_vec(&body)?;
                let uri = format!("/runtime/invocation/{}/response", ctx.id).parse::<Uri>()?;
                let req = Request::builder()
                    .uri(uri)
                    .method(Method::POST)
                    .body(Bytes::from(res))?;

                client.call(req).await?;
            }
            Err(err) => {
                let err = error(err.into());
                let err = serde_json::to_vec(&err)?;
                let uri = format!("/runtime/invocation/{}/error", ctx.id).parse::<Uri>()?;
                let req = Request::builder()
                    .uri(uri)
                    .method(Method::POST)
                    .body(Bytes::from(err))?;

                client.call(req).await?;
            }
        }
    }

    Ok(())
}

#[runtime::test]
async fn get_next() -> Result<(), Err> {
    async fn test_fn(req: String) -> Result<String, Err> {
        Ok(req)
    }

    let test_fn = handler_fn(test_fn);
    let _ = run(test_fn).await?;

    Ok(())
}
