#![feature(async_await)]
#![deny(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![warn(missing_docs, nonstandard_style, rust_2018_idioms)]

//! This is the core of the Lambda Runtime.
use crate::requests::{InvocationErrRequest, InvocationRequest, NextEventRequest};
use bytes::Bytes;
use failure::format_err;
use futures::{
    future::BoxFuture,
    prelude::*,
    task::{Context, Poll},
};
use http::{
    uri::{Authority, Scheme},
    Request, Response, Uri,
};
use serde::{Deserialize, Serialize};
use std::{
    convert::{TryFrom, TryInto},
    env,
    marker::Unpin,
    mem,
    pin::Pin,
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

pub use lambda_attributes::lambda;

mod requests;
/// A module containing various types availible a Lambda function.
pub mod types;

type Err = Box<dyn std::error::Error + Send + Sync + 'static>;

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

#[derive(Debug, PartialEq)]
struct Client {
    base: (Scheme, Authority),
}

impl Client {
    fn new(scheme: Scheme, authority: Authority) -> Self {
        Self {
            base: (scheme, authority),
        }
    }
}

/// A trait modeling interactions with the [Lambda Runtime API](https://docs.aws.amazon.com/lambda/latest/dg/runtimes-api.html).
trait EventClient<'a>: Send + Sync + Unpin {
    /// A future containing the next event from the Lambda Runtime API.
    type Fut: Future<Output = Result<Response<Bytes>, Err>> + Send + 'a;
    fn get(&self, req: Request<()>) -> Self::Fut;
    fn post(&self, req: Request<Bytes>) -> Self::Fut;
}

impl<'a> EventClient<'a> for Client {
    type Fut = BoxFuture<'a, Result<Response<Bytes>, Err>>;
    fn get(&self, _req: Request<()>) -> Self::Fut {
        let fut = async move {
            let res = Response::builder().body(Bytes::new()).unwrap();
            Ok(res)
        };
        fut.boxed()
    }

    fn post(&self, _req: Request<Bytes>) -> Self::Fut {
        let fut = async move {
            let res = Response::builder().body(Bytes::new()).unwrap();
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
        let req = NextEventRequest::new().try_into().unwrap();
        Box::pin(self.client.get(req))
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

impl<F, In, Out, E, Fut> Handler<In, Out> for HandlerFn<F>
where
    F: Fn(In) -> Fut,
    In: for<'de> Deserialize<'de>,
    Out: Serialize,
    E: Into<Err>,
    Fut: Future<Output = Result<Out, E>> + Send,
{
    type Err = E;
    type Fut = Fut;
    fn call(&mut self, req: In) -> Self::Fut {
        (self.f)(req)
    }
}

impl<F, In, Out, E, Fut> HttpHandler<In, Out> for HandlerFn<F>
where
    F: Fn(Request<In>) -> Fut,
    In: for<'de> Deserialize<'de>,
    Out: Serialize,
    E: Into<Err>,
    Fut: Send + Future<Output = Result<Response<Out>, E>>,
{
    type Err = E;
    type Fut = Fut;
    fn call(&mut self, req: Request<In>) -> Self::Fut {
        (self.f)(req)
    }
}

/// A trait describing an asynchronous function from `In` to `Out`. `In` and `Out` must implement [`Deserialize`](serde::Deserialize) and [`Serialize`](serde::Serialize).
pub trait Handler<In, Out>
where
    In: for<'de> Deserialize<'de>,
    Out: Serialize,
{
    /// Errors returned by this handler.
    type Err: Into<Err>;
    /// The future response value of this handler.
    type Fut: Future<Output = Result<Out, Self::Err>>;
    /// Process the incoming event and return the response asynchronously.
    fn call(&mut self, event: In) -> Self::Fut;
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
    let scheme = parts.scheme.ok_or(format_err!("scheme not found"))?;
    let authority = parts.authority.ok_or(format_err!("authority not found"))?;

    let client = Client::new(scheme, authority);
    let mut stream = EventStream::new(&client);

    while let Some(event) = stream.next().await {
        let (parts, body) = event?.into_parts();
        let ctx = types::LambdaCtx::try_from(parts.headers)?;
        let body = serde_json::from_slice(&body)?;

        match handler.call(body).await {
            Ok(res) => {
                let res = serde_json::to_vec(&res)?;
                let req = InvocationRequest::from_components(ctx.aws_request_id, res.into())?
                    .try_into()?;
                client.post(req).await?;
            }
            Err(e) => {
                let e = error(e.into());
                let req =
                    InvocationErrRequest::from_components(ctx.aws_request_id, e)?.try_into()?;
                client.post(req).await?;
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
    let scheme = parts.scheme.ok_or(format_err!("scheme not found"))?;
    let authority = parts.authority.ok_or(format_err!("authority not found"))?;

    let client = Client::new(scheme, authority);
    let mut stream = EventStream::new(&client);

    while let Some(event) = stream.next().await {
        let (parts, body) = event?.into_parts();
        let ctx = types::LambdaCtx::try_from(parts.headers)?;
        let body = serde_json::from_slice(&body)?;
        let req = Request::new(body);

        match handler.call(req).await {
            Ok(res) => {
                let (_parts, body) = res.into_parts();
                let body = serde_json::to_vec(&body)?;
                let req = InvocationRequest::from_components(ctx.aws_request_id, body.into())?
                    .try_into()?;
                client.post(req).await?;
            }
            Err(e) => {
                let e = error(e.into());
                let req =
                    InvocationErrRequest::from_components(ctx.aws_request_id, e)?.try_into()?;
                client.post(req).await?;
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
