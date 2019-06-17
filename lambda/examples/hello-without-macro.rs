#![feature(async_await)]

use lambda::handler_fn;
type Err = Box<dyn std::error::Error + Send + Sync + 'static>;

#[runtime::main]
async fn main() -> Result<(), Err> {
    let func = handler_fn(func);
    lambda::run(func).await?;
    Ok(())
}

async fn func(event: String) -> Result<String, Err> {
    // note the absense of a context—the context will
    // be propogated through a task local.
    Ok(event)
}
