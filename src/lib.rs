#![feature(async_await)]
#![deny(clippy::all, clippy::pedantic)]
#![warn(
    // missing_debug_implementations,
    missing_docs,
    nonstandard_style,
    rust_2018_idioms
)]

//! This is the core of the Lambda Runtime.
use futures::future::BoxFuture;
use futures::prelude::*;
use futures::task::{Context, Poll};
use http::{Request, Response};
use std::marker::Unpin;
use std::pin::Pin;

type Err = Box<dyn ::std::error::Error + Send + Sync + 'static>;

pub trait Client<'a>: Send + Sync + Unpin {
    type Fut: Future<Output = String> + Send + 'a;
    fn call(&self, req: String) -> Self::Fut;
}

struct Actual;

impl<'a> Client<'a> for Actual {
    type Fut = BoxFuture<'a, String>;
    fn call(&self, req: String) -> Self::Fut {
        let fut = async move { req };
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
    T: Client<'a>,
{
    current: Option<BoxFuture<'a, String>>,
    client: T,
}

impl<'a, T> EventStream<'a, T>
where
    T: Client<'a>,
{
    fn new(inner: T) -> Self {
        Self {
            current: None,
            client: inner,
        }
    }

    fn next_event(&self) -> BoxFuture<'a, String> {
        let res = { self.client.call(String::from("hello")) };
        Box::pin(res)
    }
}

#[must_use = "streams do nothing unless you `.await` or poll them"]
impl<'a, T> Stream for EventStream<'a, T>
where
    T: Client<'a>,
{
    type Item = String;

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
    let mut stream = EventStream::new(Actual);
    if let Some(event) = stream.next().await {
        println!("{:?}", event);
    }

    Ok(())
}
