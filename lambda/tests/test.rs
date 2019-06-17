#![feature(async_await)]

use trybuild::TestCases;

#[test]
fn tests() {
    let t = TestCases::new();
    t.compile_fail("tests/ui/parse.rs");
    t.pass("tests/ui/parse-one-arg.rs");
    t.compile_fail("tests/ui/non-async-main.rs")
}
