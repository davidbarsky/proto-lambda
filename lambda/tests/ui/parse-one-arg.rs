#![feature(async_await)]

use lambda::lambda;
type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

#[lambda]
#[runtime::main]
async fn main(s: String) -> Result<String, Error> {
    dbg!(&s);
    Ok(s)
}
