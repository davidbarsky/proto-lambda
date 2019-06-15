use crate::{Err, ErrorReport};
use bytes::Bytes;
use http::{uri::PathAndQuery, Method, Request, Uri};
use std::convert::TryInto;

/// Represents a request that retrieves an invocation event. The response body contains
/// the payload from the invocation, which is a JSON document that contains event data
/// from the function trigger. The response headers contain additional data about the invocation.
///
/// ```
/// use std::convert::TryInto;
/// use http::Request;
/// use lambda_proto::requests::NextEventRequest;
///
/// let req = NextEventRequest::new();
/// let req: Request<()> = req.try_into()?;
/// # Ok::<(), http::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct NextEventRequest {
    path_and_query: PathAndQuery,
}

impl NextEventRequest {
    /// Constructs a new `NextEventRequest`.
    pub fn new() -> Self {
        Self {
            path_and_query: PathAndQuery::from_static("/2018-06-01/runtime/invocation/next"),
        }
    }
}

impl TryInto<Request<()>> for NextEventRequest {
    type Error = http::Error;

    fn try_into(self) -> Result<Request<()>, Self::Error> {
        let uri = Uri::builder().path_and_query(self.path_and_query).build()?;
        let req = Request::builder().method(Method::GET).uri(uri).body(())?;
        Ok(req)
    }
}

/// A request that returns a successful response to Lambda. The response body payload
///
///
/// ```
/// use bytes::Bytes;
/// use std::convert::TryInto;
/// use http::Request;
/// use lambda_proto::requests::InvocationRequest;
///
/// let id = String::from("123");
/// let response = Bytes::from("bye!");
/// let req = InvocationRequest::from_components(id, response)?;
/// let req: Request<Bytes> = req.try_into()?;
/// # Ok::<(), failure::Error>(())
/// ```
pub struct InvocationRequest {
    path_and_query: PathAndQuery,
    response: Bytes,
}

impl InvocationRequest {
    /// Fallibly constructs an `InvocationRequest`.
    pub fn from_components(request_id: String, response: Bytes) -> Result<Self, Err> {
        let path = format!("2018-06-01/runtime/invocation/{}/response", request_id);
        let path_and_query = PathAndQuery::from_shared(Bytes::from(path))?;
        let req = InvocationRequest {
            path_and_query,
            response,
        };
        Ok(req)
    }
}

impl TryInto<Request<Bytes>> for InvocationRequest {
    type Error = http::Error;

    fn try_into(self) -> Result<Request<Bytes>, Self::Error> {
        let uri = Uri::builder().path_and_query(self.path_and_query).build()?;
        let req = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .body(self.response)?;
        Ok(req)
    }
}

/// A request that returns a successful response to Lambda. The response body payload
///
///
/// ```
/// use bytes::Bytes;
/// use std::convert::TryInto;
/// use http::Request;
/// use lambda_proto::requests::InvocationErrRequest;
///
/// let id = String::from("123");
/// let response = Bytes::from("bye!");
/// let req = InvocationErrRequest::from_components(id, response)?;
/// let req: Request<Bytes> = req.try_into()?;
/// # Ok::<(), failure::Error>(())
/// ```
pub struct InvocationErrRequest {
    path_and_query: PathAndQuery,
    response: ErrorReport,
}

impl InvocationErrRequest {
    /// Fallibly constructs an `InvocationErrRequest`.
    pub fn from_components(request_id: String, response: ErrorReport) -> Result<Self, Err> {
        let path = format!("2018-06-01/runtime/invocation/{}/error", request_id);
        let path_and_query = PathAndQuery::from_shared(Bytes::from(path))?;
        let req = InvocationErrRequest {
            path_and_query,
            response,
        };
        Ok(req)
    }
}

impl TryInto<Request<Bytes>> for InvocationErrRequest {
    type Error = Err;

    fn try_into(self) -> Result<Request<Bytes>, Self::Error> {
        let uri = Uri::builder().path_and_query(self.path_and_query).build()?;
        let body = serde_json::to_vec(&self.response)?;
        let req = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .body(body.into())?;
        Ok(req)
    }
}
