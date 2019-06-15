#![feature(async_await)]

use lambda::handler_fn;
use lambda_proto as lambda;

type Err = Box<dyn std::error::Error + Send + Sync + 'static>;

#[runtime::main]
async fn main() -> Result<(), Err> {
    let f = handler_fn(run);
    lambda::run(f).await?;
    Ok(())
}

async fn run(event: String) -> Result<String, Err> {
    Ok(event)
}
