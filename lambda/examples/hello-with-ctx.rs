#![feature(async_await)]

use lambda::{lambda, LambdaCtx};
type Err = Box<dyn std::error::Error + Send + Sync + 'static>;

#[lambda]
#[runtime::main]
async fn main(event: String, ctx: LambdaCtx) -> Result<String, Err> {
    let _ = ctx;
    Ok(event)
}
