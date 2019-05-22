#![feature(async_await)]
#![deny(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![warn(
    // missing_debug_implementations,
    missing_docs,
    nonstandard_style,
    rust_2018_idioms
)]

//! This is the core of the Lambda Runtime.
use envy;
use failure::{format_err, Error as Err};
use futures::{
    future::BoxFuture,
    prelude::*,
    task::{Context, Poll},
};
use http::{
    uri::{Authority, PathAndQuery, Scheme},
    Method, Request, Response, Uri,
};
use serde::{Deserialize, Serialize};
use std::{marker::Unpin, pin::Pin};

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
    pub fn from_env() -> Self {
        envy::from_env::<Config>().unwrap()
    }
}

struct Client<T> {
    base: (Scheme, Authority),
    _marker: std::marker::PhantomData<T>,
}

impl<T> Client<T> {
    fn new(scheme: Scheme, authority: Authority) -> Self {
        Self {
            base: (scheme, authority),
            _marker: std::marker::PhantomData,
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

/// Fetches the next event from Lambda's
pub trait EventClient<'a, T>: Send + Sync + Unpin {
    type Fut: Future<Output = Result<Response<T>, Err>> + Send + 'a;
    fn call(&self, req: Request<T>) -> Self::Fut;
}

impl<'a, T> EventClient<'a, T> for Client<T>
where
    T: Send + Sync + Unpin + 'a,
{
    type Fut = BoxFuture<'a, Result<Response<T>, Err>>;
    fn call(&self, req: Request<T>) -> Self::Fut {
        let fut = async move { Ok(Response::new(req.into_body())) };
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
pub struct EventStream<'a, T, U>
where
    T: EventClient<'a, U>,
{
    current: Option<BoxFuture<'a, Result<Response<U>, Err>>>,
    client: &'a T,
}

impl<'a, T, U> EventStream<'a, T, U>
where
    T: EventClient<'a, U>,
    U: Default,
{
    fn new(inner: &'a T) -> Self {
        Self {
            current: None,
            client: inner,
        }
    }

    fn next_event(&self) -> BoxFuture<'a, Result<Response<U>, Err>> {
        let req = make_next_event_request().unwrap();
        let fut = self.client.call(req);
        Box::pin(fut)
    }
}

fn make_next_event_request<T: Default>() -> Result<Request<T>, Err> {
    let uri = Uri::builder()
        .path_and_query("/2018-06-01/runtime/invocation/next")
        .build()?;
    let req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Default::default())?;
    Ok(req)
}

fn make_init_err() -> Result<Request<String>, Err> {
    let uri = Uri::builder()
        .path_and_query("/2018-06-01/runtime/invocation/next")
        .build()?;
    let req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Default::default())?;
    Ok(req)
}

fn ok_resp(event_id: String) -> Result<Request<String>, Err> {
    let query: &str = &format!("/2018-06-01/runtime/invocation/{}/response", event_id);
    let uri = Uri::builder().path_and_query(query).build()?;
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .body(Default::default())?;
    Ok(req)
}

fn err_resp(event_id: String) -> Result<Request<String>, Err> {
    let query: &str = &format!("/2018-06-01/runtime/invocation/{}/error", event_id);
    let uri = Uri::builder().path_and_query(query).build()?;
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .body(Default::default())?;
    Ok(req)
}

#[must_use = "streams do nothing unless you `.await` or poll them"]
impl<'a, T, U> Stream for EventStream<'a, T, U>
where
    T: EventClient<'a, U>,
    U: Default,
{
    type Item = Result<Response<U>, Err>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // The `loop` is used to drive the inner future (`current`) to completion, advancing
        // the state of this stream to yield a new `Item`. Loops like the one below are
        // common in many hand-implemented `Futures` and `Streams`.
        loop {
            // The stream first checks an inner future is set. If the future is present,
            // the a futures runtime like Tokio polls the inner future to completition.
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
                    // Otherwise, the future signals that it's not ready, so we do the same
                    // to the Tokio runtime.
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
    let config = Config::from_env();

    let uri: Uri = config.endpoint.parse::<Uri>()?;
    let parts = uri.into_parts();
    let scheme = parts.scheme.ok_or(format_err!("scheme not found"))?;
    let authority = parts.authority.ok_or(format_err!("authority not found"))?;

    let client: Client<String> = Client::new(scheme, authority);
    let mut stream = EventStream::new(&client);
    if let Some(event) = stream.next().await {
        // pretend handling of an event!
        let (_headers, body) = event?.into_parts();
        match handle(body) {
            Ok(res) => {
                let req = ok_resp(res)?;
                client.call(req).await?;
            }
            Err(e) => {
                let req = err_resp(e.to_string())?;
                client.call(req).await?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
fn handle(event: String) -> Result<String, Err> {
    Ok(event)
}
