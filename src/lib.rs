#![feature(async_await)]
#![deny(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![warn(missing_docs, nonstandard_style, rust_2018_idioms)]

//! This is the core of the Lambda Runtime.
use crate::requests::{InvocationErrRequest, InvocationRequest, NextEventRequest};
use bytes::Bytes;
use envy;
use failure::{bail, format_err, Error as Err};
use futures::{
    future::{BoxFuture, IntoFuture},
    prelude::*,
    task::{Context, Poll},
};
use http::{
    uri::{Authority, PathAndQuery, Scheme},
    Method, Request, Response, Uri,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    convert::{TryFrom, TryInto},
    marker::Unpin,
    pin::Pin,
};

pub mod requests;
pub mod types;

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Config {
    #[serde(rename = "AWS_LAMBDA_RUNTIME_API")]
    pub endpoint: String,
    #[serde(rename = "AWS_LAMBDA_FUNCTION_NAME")]
    pub function_name: String,
    #[serde(rename = "AWS_LAMBDA_FUNCTION_MEMORY_SIZE")]
    pub memory: i32,
    #[serde(rename = "AWS_LAMBDA_FUNCTION_VERSION")]
    pub version: String,
    #[serde(rename = "AWS_LAMBDA_LOG_STREAM_NAME")]
    pub log_stream: String,
    #[serde(rename = "AWS_LAMBDA_LOG_GROUP_NAME")]
    pub log_group: String,
}

impl Config {
    pub fn from_env() -> Result<Self, Err> {
        let conf = envy::from_env::<Config>()?;
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

    fn add_origin(&self, query: PathAndQuery) -> Uri {
        Uri::builder()
            .scheme(self.base.0.clone())
            .authority(self.base.1.clone())
            .path_and_query(query)
            .build()
            .unwrap()
    }
}

/// A client responsible for interacting with the [Lambda Runtime API](https://docs.aws.amazon.com/lambda/latest/dg/runtimes-api.html).
pub trait EventClient<'a>: Send + Sync + Unpin {
    type Fut: Future<Output = Result<Response<Bytes>, Err>> + Send + 'a;
    fn get(&self, req: Request<()>) -> Self::Fut;
    fn post(&self, req: Request<Bytes>) -> Self::Fut;
}

impl<'a> EventClient<'a> for Client {
    type Fut = BoxFuture<'a, Result<Response<Bytes>, Err>>;
    fn get(&self, req: Request<()>) -> Self::Fut {
        let fut = async move {
            let res = Response::builder().body(Bytes::new()).unwrap();
            Ok(res)
        };
        fut.boxed()
    }

    fn post(&self, req: Request<Bytes>) -> Self::Fut {
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
pub struct EventStream<'a, T>
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
        let fut = self.client.get(req);
        Box::pin(fut)
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
                        self.current = Some(self.next_event());
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

#[runtime::test]
async fn get_next() -> Result<(), Err> {
    let mut config = Config::default();
    config.endpoint = String::from("http://localhost:8000");

    let uri: Uri = config.endpoint.parse::<Uri>()?;
    let parts = uri.into_parts();
    let scheme = parts.scheme.ok_or(format_err!("scheme not found"))?;
    let authority = parts.authority.ok_or(format_err!("authority not found"))?;

    let client = Client::new(scheme, authority);
    let mut stream = EventStream::new(&client);
    while let Some(event) = stream.next().await {
        let (parts, body) = event?.into_parts();
        let ctx = types::Context::try_from(parts.headers)?;

        let f = |bytes| bytes;

        match f(body) {
            Ok(res) => {
                let req =
                    InvocationRequest::from_components(ctx.aws_request_id, res)?.try_into()?;
                client.post(req).await?;
            }
            Err(e) => {
                let req = InvocationErrRequest::from_components(ctx.aws_request_id, Bytes::new())?
                    .try_into()?;
                client.post(req).await?;
            }
        }
    }

    Ok(())
}
