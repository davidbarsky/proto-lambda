#![feature(async_await)]

use lambda::lambda;
type Err = Box<dyn std::error::Error + Send + Sync + 'static>;

#[lambda]
#[runtime::main]
async fn main(s: String) -> Result<String, Err> {
    Ok(s)
}
