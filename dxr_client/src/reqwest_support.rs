#[cfg(feature = "multicall")]
use std::collections::HashMap;
use std::fmt::Debug;
use std::io::{Error, ErrorKind};
use std::io::prelude::*;
use std::net::ToSocketAddrs;
use std::ops::Add;
use std::path::Path;
use std::rc::Rc;

use bytes::{BufMut, BytesMut};
use futures::{SinkExt, StreamExt};
use http::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use http::request;
use log::error;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};
use tokio_scgi::client::{SCGICodec, SCGIRequest};
use tokio_util::codec::Framed;
use url::Url;

use dxr::{DxrError, Fault, FaultResponse, MethodCall, MethodResponse, TryFromValue, TryToParams};
#[cfg(feature = "multicall")]
use dxr::Value;

use crate::{Call, DEFAULT_USER_AGENT};

/// Error type for XML-RPC clients based on [`reqwest`].
#[derive(Debug, Error)]
pub enum ClientError {
    /// Error variant for XML-RPC server faults.
    #[error("{}", fault)]
    Fault {
        /// Fault returned by the server.
        #[from]
        fault: Fault,
    },
    /// Error variant for XML-RPC errors.
    #[error("{}", error)]
    RPC {
        /// XML-RPC parsing error.
        #[from]
        error: DxrError,
    },
    /// Error variant for networking errors.
    #[error("{}", error)]
    Net {
        /// Networking error returned by [`reqwest`].
        #[from]
        error: reqwest::Error,
    },
}

/// Builder that takes parameters for constructing a [`Client`] based on [`reqwest::Client`].
#[derive(Debug)]
pub struct ClientBuilder {
    url: Url,
    headers: HeaderMap,
    user_agent: Option<&'static str>,
}

impl ClientBuilder {
    /// Constructor for [`ClientBuilder`] from the URL of the XML-RPC server.
    ///
    /// This also sets up the default `Content-Type: text/xml` HTTP header for XML-RPC requests.
    pub fn new(url: Url) -> Self {
        let mut default_headers = HeaderMap::new();
        default_headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/xml"));

        ClientBuilder {
            url,
            headers: default_headers,
            user_agent: None,
        }
    }

    /// Method for overriding the default User-Agent header.
    pub fn user_agent(mut self, user_agent: &'static str) -> Self {
        self.user_agent = Some(user_agent);
        self
    }

    /// Method for providing additional custom HTTP headers.
    ///
    /// Using [`HeaderName`] constants for the header name is recommended. The [`HeaderValue`]
    /// argument needs to be parsed (probably from a string) with [`HeaderValue::from_str`] to
    /// ensure their value is valid.
    pub fn add_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }

    /// Build the [`Client`] by setting up and initializing the internal [`reqwest::Client`].
    ///
    /// If no custom value was provided for `User-Agent`, the default value
    /// ([`DEFAULT_USER_AGENT`]) will be used.
    pub fn build(self) -> Client {
        let user_agent = self.user_agent.unwrap_or(DEFAULT_USER_AGENT);

        let builder = self.add_header(USER_AGENT, HeaderValue::from_static(user_agent));

        let client = reqwest::Client::builder()
            .default_headers(builder.headers)
            .build()
            .expect("Failed to initialize reqwest client.");

        Client {
            url: builder.url,
            client,
        }
    }
}

/// # XML-RPC client implementation
///
/// This type provides a very simple XML-RPC client implementation based on [`reqwest`]. Initialize
/// the [`Client`], submit a [`Call`], get a result (or a fault).
#[derive(Debug)]
pub struct Client {
    url: Url,
    client: reqwest::Client,
}

impl Client {
    /// Constructor for a [`Client`] from a [`reqwest::Client`] that was already initialized.
    pub fn with_client(url: Url, client: reqwest::Client) -> Self {
        Client { url, client }
    }

    /// Asynchronous method for handling remote procedure calls with XML-RPC.
    ///
    /// Fault responses from the XML-RPC server are transparently converted into [`Fault`] errors.
    /// Invalid XML-RPC responses or faults will result in an appropriate [`DxrError`].
    pub async fn call<P: TryToParams, R: TryFromValue>(&self, call: Call<'_, P, R>) -> Result<R, ClientError> {
        // serialize XML-RPC method call
        let request = call.as_xml_rpc()?;
        let body = request_to_body(&request)?;

        let response = match self.url.clone().scheme() {
            "unix" => {
                let path = Path::new(self.url.path());
                let req = SCGIRequest::Request (
                    vec![
                        ("CONTENT_LENGTH".to_owned(), body.len().to_string().to_owned()),
                        ("SCGI".to_owned(), "1".to_owned()),
                        ("REQUEST_METHOD".to_owned(), "POST".to_owned()),
                        ("REQUEST_URI".to_owned(), "/RPC".to_owned()),
                    ],
                    BytesMut::from(body.as_bytes())
                );

                match send_scgi_request(self.url.path(), req).await {
                    Ok(mut stream) => {
                        /*stream.write_all(body.as_bytes()).unwrap();
                        let mut buf = String::new();
                        stream.read_to_string(&mut buf).unwrap();
                        buf*/
                        // println!("Response: {:?}", stream);
                        stream
                    }
                    Err(e) => {
                        eprintln!("Raw Error OS Code: {:?}", e.raw_os_error());
                        eprintln!("Failed to connect to rtorrent socket: {:?}", e);
                        return Err(ClientError::Fault { fault: Fault::new(1, "Failed to connect to rtorrent socket".to_string())});
                    }
                }
            }
            _ => {
                // let request = self.client.post(self.url.clone()).body(body).build()?;
                let request = match self.client.post(self.url.clone()).body(body).build() {
                    Ok(request) => request,
                    Err(e) => {
                        eprintln!("Failed to build the request: {:?}", e);
                        return Err(ClientError::Net { error: e });
                    }
                };
                self.client.execute(request).await?.text().await?
            }
        };
        // construct request and send to server

        use std::io::prelude::*;

        async fn send_scgi_request(socket_path: &str, request: SCGIRequest) -> std::io::Result<String> {
            // Connect to the SCGI server
            let addr = Path::new(socket_path);
            let mut client = UnixStream::connect(&addr).await?;
            let mut framed = Framed::new(client, SCGICodec::new());
            // Send request
            framed.send(request).await?;

            let mut none_count = 0;
            let mut some_count = 0;
            let mut err_count = 0;
            let mut resp = String::new();

            loop {
                match framed.next().await {
                    None => {
                        // SCGI response not ready: loop for more rx data
                        // Shouldn't happen for response data, but this is how it would work...
                        none_count += 1;
                        eprintln!("Response data is incomplete, resuming read");
                    }
                    Some(Err(e)) => {
                        err_count += 1;
                        // RX error: return error and abort
                        return Err(Error::new(
                            ErrorKind::Other,
                            format!("Error when waiting for response: {}", e),
                        ));
                    }
                    Some(Ok(response)) => {
                        // Got SCGI response: if empty, treat as end of response.
                        some_count += 1;
                        if response.len() == 0 {
                            break;
                        }
                        let mut res = response.to_owned();
                        match tokio_util::codec::Decoder::decode(&mut SCGICodec::new(), &mut res){
                            Ok(Some(s)) => {
                                match String::from_utf8(response.to_vec()) {
                                    Ok(s) => {
                                        //remove text until <?xml version="1.0" encoding="UTF-8"?> to fix invalid xml
                                        let mut s2 = s.split("<?xml").collect::<Vec<&str>>()[1].to_owned();
                                        &s2.insert_str(0, "<?xml");
                                        //println!("Got response: {}", s);
                                        /*println!("Got response: {}", s);
                                        resp.push_str(s.as_str());*/
                                        // println!("Got response: {}", s2);
                                        resp.push_str(s2.as_str());
                                    },
                                    Err(e) => {
                                        eprintln!(
                                            "{} byte response is not UTF8 ({}):\n{:?}",
                                            response.len(),
                                            e,
                                            response
                                        );
                                    },
                                }
                            },
                            Err(e) => {
                                eprintln!(
                                    "{} byte response is not UTF8 ({}):\n{:?}",
                                    response.len(),
                                    e,
                                    response
                                );
                            },
                            Ok(None) => {
                                eprintln!("Response data is incomplete, resuming read");
                            }
                        }
                    }
                }
            }

            Ok(resp)
        }
        /*
        CONTENT_LENGTH 179
        SCGI 1
        REQUEST_METHOD POST
        REQUEST_URI /RPC
        */
        // deserialize XML-RPC method response
        let contents = response;
        let result = response_to_result(&contents)?;

        // extract return value
        Ok(R::try_from_value(&result.inner())?)
    }

    /// Asynchronous method for handling "system.multicall" calls.
    ///
    /// *Note*: This method does not check if the number of method calls matches the number of
    /// returned results.
    #[cfg(feature = "multicall")]
    pub async fn multicall<P: TryToParams>(
        &self,
        call: Call<'_, P, Vec<Value>>,
    ) -> Result<Vec<Result<Value, Fault>>, ClientError> {
        let response = self.call(call).await?;

        let mut results = Vec::new();
        for result in response {
            // return values for successful calls are arrays that contain a single value
            if let Ok((value, )) = <(Value, )>::try_from_value(&result) {
                results.push(Ok(value));
            };

            // return values for failed calls are structs with two members
            if let Ok(mut value) = <HashMap<String, Value>>::try_from_value(&result) {
                let code = match value.remove("faultCode") {
                    Some(code) => code,
                    None => return Err(DxrError::missing_field("Fault", "faultCode").into()),
                };

                let string = match value.remove("faultString") {
                    Some(string) => string,
                    None => return Err(DxrError::missing_field("Fault", "faultString").into()),
                };

                // The value might still contain other struct fields:
                // Rather than return an error because they are unexpected, they are ignored,
                // since the required "faultCode" and "faultString" members were present.

                let fault = Fault::new(i32::try_from_value(&code)?, String::try_from_value(&string)?);
                results.push(Err(fault));
            }
        }

        Ok(results)
    }
}

fn request_to_body(call: &MethodCall) -> Result<String, DxrError> {
    let body = [
        r#"<?xml version="1.0"?>"#,
        dxr::serialize_xml(&call)
            .map_err(|error| DxrError::invalid_data(error.to_string()))?
            .as_str(),
        "",
    ]
        .join("\n");

    Ok(body)
}

fn response_to_result(contents: &str) -> Result<MethodResponse, ClientError> {
    // need to check for FaultResponse first:
    // - a missing <params> tag is ambiguous (can be either an empty response, or a fault response)
    // - a present <fault> tag is unambiguous
    let error2 = match dxr::deserialize_xml(contents) {
        Ok(fault) => {
            let response: FaultResponse = fault;
            return match Fault::try_from(response) {
                // server fault: return Fault
                Ok(fault) => Err(fault.into()),
                // malformed server fault: return DxrError
                Err(error) => Err(error.into()),
            };
        }
        Err(error) => error.to_string(),
    };

    let error1 = match dxr::deserialize_xml(contents) {
        Ok(response) => return Ok(response),
        Err(error) => error.to_string(),
    };

    // log errors if the contents could not be deserialized as either response or fault
    log::debug!("Failed to deserialize response as either value or fault.");
    log::debug!("Response failed with: {}; Fault failed with: {}", error1, error2);

    // malformed response: return DxrError::InvalidData
    Err(DxrError::invalid_data(contents.to_owned()).into())
}
