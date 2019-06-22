use std::{
    mem, ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::Err;

static HOOK: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// A computer-readable report of an unhandled error.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct ErrorReport {
    /// The type of the error passed to the Lambda APIs.
    pub name: String,
    /// The [std::fmt::Display] output of the error.
    pub err: String,
}

fn default_error_hook(err: Err) -> ErrorReport {
    ErrorReport {
        name: String::from("UnknownError"),
        err: format!("{}", err),
    }
}

/// Transforms
///
/// This function is called by the Lambda Runtime if an error is returned from a Handler.
/// This implementation is a near-direct copy of [`std::alloc::set_alloc_error_hook`], down
/// to the transmute operation.
pub(crate) fn generate_report(err: Err) -> ErrorReport {
    let hook = HOOK.load(Ordering::SeqCst);
    let hook: fn(Err) -> ErrorReport = if hook.is_null() {
        default_error_hook
    } else {
        unsafe { mem::transmute(hook) }
    };
    hook(err)
}

/// Registers a custom error hook, replacing any that was previously registered.
///
/// The Lambda error hook is invoked when a [`Handler`] or [`HttpHandler`] returns an error, but prior
/// to the runtime reporting the error to the Lambda Runtime APIs. This hook is intended to be used
/// by those interested in programatialy
///
/// This function, in terms of intended usage and implementation, mimics [`std::alloc::set_alloc_error_hook`].
/// 
/// # Example
/// ```
/// #![feature(async_await)]
/// 
/// use lambda::{handler_fn, error_hook};
/// type Err = Box<dyn std::error::Error + Send + Sync + 'static>;
/// 
/// #[runtime::main]
/// async fn main() -> Result<(), Err> {
///     let func = handler_fn(func);
///     error_hook::set_error_hook(error_hook);
///     lambda::run(func).await?;
///     Ok(())
/// }
///
/// async fn func(event: String) -> Result<String, Err> {
///     Ok(event)
/// }
///
/// fn error_hook(e: Err) -> error_hook::ErrorReport {
///     error_hook::ErrorReport{
///         name: String::from("CustomError"),
///         err: format!("{}", e),
///     }
/// }
/// ```
pub fn set_error_hook<E: Into<Err>>(hook: fn(err: E) -> ErrorReport) {
    HOOK.store(hook as *mut (), Ordering::SeqCst);
}

#[test]
fn set_err_hook() {
    use crate::err_fmt;
    set_error_hook(|err: Err| {
        if let Some(e) = err.downcast_ref::<std::io::Error>() {
            ErrorReport {
                name: String::from("std::io::Error"),
                err: format!("{}", e),
            }
        } else {
            default_error_hook(err)
        }
    });

    let e = err_fmt!("An error");
    let e = generate_report(e.into());
    assert_eq!(String::from("UnknownError"), e.name)
}
