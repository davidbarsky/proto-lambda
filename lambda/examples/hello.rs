#![feature(async_await)]

use lambda::{lambda, LambdaCtx};
type Err = Box<dyn std::error::Error + Send + Sync + 'static>;

#[lambda]
#[runtime::main]
async fn main(event: String, _ctx: LambdaCtx) -> Result<String, Err> {
    Ok(event)
}
