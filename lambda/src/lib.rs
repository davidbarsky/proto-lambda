#![feature(async_await)]
#![deny(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![warn(missing_docs, nonstandard_style, rust_2018_idioms)]

//! This is the core of the Lambda Runtime.
use bytes::Bytes;
use futures::prelude::*;
use http::{response::Parts, Method, Request, Response, Uri};
use serde::{Deserialize, Serialize};
use std::{convert::TryFrom, env};

pub use lambda_attributes::lambda;

mod client;
/// Mechanism to provide a custom error reporting hook.
pub mod error_hook;
/// Types availible to a Lambda function.
pub mod types;
use crate::{error_hook::generate_report, types::LambdaCtx};

use client::{Client, EventClient, EventStream};

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
    fn call(&mut self, event: A) -> Self::Fut;
}

pub trait HttpHandler<A, B>: Handler<Request<A>, Response<B>>
where
    Request<A>: for<'de> Deserialize<'de>,
    Response<B>: Serialize,
{
    /// Process the incoming request and return the response asynchronously.
    fn call_http(&mut self, event: Request<A>) -> <Self as Handler<Request<A>, Response<B>>>::Fut;
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
    fn call(&mut self, req: A) -> Self::Fut {
        // we pass along the context here
        (self.f)(req)
    }
}

impl<F, A, B, E, Fut> HttpHandler<A, B> for HandlerFn<F>
where
    F: Fn(Request<A>) -> Fut,
    Request<A>: for<'de> Deserialize<'de>,
    Response<B>: Serialize,
    E: Into<Err>,
    Fut: Send + Future<Output = Result<Response<B>, E>>,
{
    fn call_http(&mut self, req: Request<A>) -> Self::Fut {
        (self.f)(req)
    }
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

        match handler.call(body).await {
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
                let err = error_hook::generate_report(err.into());
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
// pub async fn run_http<F, A, B>(mut handler: F) -> Result<(), Err>
// where
//     F: HttpHandler<A, B>,
//     Request<A>: for<'de> Deserialize<'de>,
//     Response<B>: Serialize,
// {
//     let mut config = Config::default();
//     config.endpoint = String::from("http://localhost:8000");
//     let parts = config.endpoint.parse::<Uri>()?.into_parts();
//     let scheme = parts.scheme.ok_or(err_fmt!("scheme not found"))?;
//     let authority = parts.authority.ok_or(err_fmt!("authority not found"))?;

//     let client = Client::new(scheme, authority);
//     let mut stream = EventStream::new(&client);

//     while let Some(event) = stream.next().await {
//         let (parts, body): (Parts, Bytes) = event?.into_parts();
//         let ctx: LambdaCtx = LambdaCtx::try_from(parts.headers)?;
//         let body = serde_json::from_slice(&body)?;
//         let req = Request::new(body);

//         match handler.call_http(req).await {
//             Ok(res) => {
//                 let (parts, body) = res.into_parts();
//                 // let res = serde_json::to_vec(&body)?;
//                 let uri = format!("/runtime/invocation/{}/response", ctx.id).parse::<Uri>()?;
//                 let req = Request::builder()
//                     .uri(uri)
//                     .method(Method::POST)
//                     .body(Bytes::from(res))?;

//                 client.call(req).await?;
//             }
//             Err(err) => {
//                 let err = generate_report(err.into());
//                 let err = serde_json::to_vec(&err)?;
//                 let uri = format!("/runtime/invocation/{}/error", ctx.id).parse::<Uri>()?;
//                 let req = Request::builder()
//                     .uri(uri)
//                     .method(Method::POST)
//                     .body(Bytes::from(err))?;

//                 client.call(req).await?;
//             }
//         }
//     }

//     Ok(())
// }

#[runtime::test]
async fn get_next() -> Result<(), Err> {
    async fn test_fn(req: String) -> Result<String, Err> {
        Ok(req)
    }

    let test_fn = handler_fn(test_fn);
    let _ = run(test_fn).await?;

    Ok(())
}
