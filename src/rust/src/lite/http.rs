//
// Copyright 2019-2022 Signal Messenger, LLC
// SPDX-License-Identifier: AGPL-3.0-only
//

//! Make calls to the App to do HTTP requests
//! and define common types like Method, Response, Client, etc.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use serde::Deserialize;

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get = 0,
    Put,
    Post,
    Delete,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: HashMap<String, String>,
    pub body: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct Response {
    pub status: ResponseStatus,
    pub body: Vec<u8>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ResponseStatus {
    pub code: u16,
}

impl From<u16> for ResponseStatus {
    fn from(code: u16) -> Self {
        Self { code }
    }
}

impl ResponseStatus {
    pub fn r#type(self) -> ResponseStatusType {
        ResponseStatusType::from_code(self.code)
    }

    pub fn is_success(self) -> bool {
        self.r#type().is_success()
    }

    pub fn is_error(self) -> bool {
        self.r#type().is_error()
    }

    pub const GROUP_CALL_NOT_STARTED: Self = Self { code: 404 };
    pub const GROUP_CALL_FULL: Self = Self { code: 413 };

    // Artificial codes not actually returned by the server
    pub const INVALID_CLIENT_AUTH: Self = Self { code: 601 };
    pub const REQUEST_FAILED: Self = Self { code: 602 };
    pub const INVALID_RESPONSE_BODY_UTF8: Self = Self { code: 701 };
    pub const INVALID_RESPONSE_BODY_JSON: Self = Self { code: 702 };
    pub const CALL_LINK_EXPIRED: Self = Self { code: 703 };
    pub const CALL_LINK_INVALID: Self = Self { code: 704 };
}

impl std::fmt::Display for ResponseStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.code.fmt(f)
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(u16)]
pub enum ResponseStatusType {
    Unknown = 0,
    Informational = 100,
    Success = 200,
    Redirection = 300,
    ClientError = 400,
    ServerError = 500,
    RequestError = 600,
    ResponseError = 700,
}

impl ResponseStatusType {
    pub fn from_code(code: u16) -> Self {
        match code {
            100..=199 => Self::Informational,
            200..=299 => Self::Success,
            300..=399 => Self::Redirection,
            400..=499 => Self::ClientError,
            500..=599 => Self::ServerError,
            600..=699 => Self::RequestError,
            700..=799 => Self::ResponseError,
            _ => Self::Unknown,
        }
    }

    pub fn is_success(self) -> bool {
        matches!(self, Self::Success)
    }

    pub fn is_error(self) -> bool {
        matches!(
            self,
            Self::ClientError | Self::ServerError | Self::RequestError | Self::ResponseError
        )
    }
}

pub fn parse_json_response<'a, D: Deserialize<'a>>(
    response: Option<&'a Response>,
) -> Result<D, ResponseStatus> {
    let response = response.ok_or(ResponseStatus::REQUEST_FAILED)?;
    if !response.status.is_success() {
        return Err(response.status);
    }
    let deserialized = serde_json::from_slice(&response.body)
        .map_err(|_| ResponseStatus::INVALID_RESPONSE_BODY_JSON)?;
    Ok(deserialized)
}

pub type ResponseCallback = Box<dyn FnOnce(Option<Response>) + Send>;

/// An abstract HTTP client
/// Rust consumers of HTTP clients should use this trait.
/// Apps should use a platform-specific Client impl.
pub trait Client {
    fn send_request(&self, request: Request, callback: ResponseCallback);
}

/// Platform-specific methods that must be provided by
/// the application to create a platform-specific Client impl.
pub trait Delegate {
    /// Responses should be provided via DelegatingClient.received_response
    fn send_request(&self, request_id: u32, request: Request);
}

/// An impl of Client that calls out to a Delegate to make requests.
#[derive(Clone)]
pub struct DelegatingClient {
    delegate: Arc<Mutex<dyn Delegate + Send>>,
    response_callbacks: Arc<Mutex<ResponseCallbacks>>,
}

impl DelegatingClient {
    pub fn new(delegate: impl Delegate + Send + 'static) -> Self {
        Self {
            delegate: Arc::new(Mutex::new(delegate)),
            response_callbacks: Arc::default(),
        }
    }

    /// A None Response indicates a failure.
    pub fn received_response(&self, request_id: u32, response: Option<Response>) {
        info!(
            "http:DelegatingClient:received_response(): request_id: {}",
            request_id
        );

        match response.as_ref() {
            Some(r) => {
                info!("  status_code: {}", r.status.code);
                debug!("  body: {} bytes", r.body.len())
            }
            None => {
                info!("  no response, which indicates request failure");
            }
        }

        let response_callback = {
            let mut response_callbacks = self
                .response_callbacks
                .lock()
                .expect("http:DelegatingClient:response_callbacks lock");
            response_callbacks.pop(request_id)
        };
        if let Some(response_callback) = response_callback {
            debug!("http:DelegatingClient:received_response(): calling registered callback");
            response_callback(response);
        } else {
            error!(
                "http:DelegatingClient:received_response(): unknown request ID: {}",
                request_id
            );
        }
    }
}

impl Client for DelegatingClient {
    fn send_request(&self, request: Request, response_callback: ResponseCallback) {
        info!("http:DelegatingClient:make_request()");
        debug!(
            "  url: {} method: {:?} headers: {:?}",
            request.url, request.method, request.headers
        );
        let request_id = {
            let mut response_callbacks = self
                .response_callbacks
                .lock()
                .expect("http:DelegatingClient:response_callbacks lock");
            response_callbacks.push(response_callback)
        };
        let delegate = self
            .delegate
            .lock()
            .expect("http:DelegatingClient:state lock");
        delegate.send_request(request_id, request)
    }
}

#[derive(Default)]
struct ResponseCallbacks {
    response_callback_by_request_id: HashMap<u32, ResponseCallback>,
    next_request_id: u32,
}

impl ResponseCallbacks {
    fn push(&mut self, response_callback: ResponseCallback) -> u32 {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        self.response_callback_by_request_id
            .insert(request_id, response_callback);
        request_id
    }

    fn pop(&mut self, request_id: u32) -> Option<ResponseCallback> {
        self.response_callback_by_request_id.remove(&request_id)
    }
}

#[cfg(any(target_os = "ios", feature = "java", feature = "check-all"))]
pub mod ios {
    use libc::{c_void, size_t};

    use crate::lite::{
        ffi::ios::{rtc_Bytes, rtc_String, FromOrDefault},
        http,
    };

    pub type Client = http::DelegatingClient;

    #[repr(C)]
    pub struct rtc_http_Delegate {
        pub retained: *mut c_void,
        pub release: extern "C" fn(retained: *mut c_void),
        pub send_request:
            extern "C" fn(unretained: *const c_void, request_id: u32, request: rtc_http_Request),
    }

    unsafe impl Send for rtc_http_Delegate {}

    impl Drop for rtc_http_Delegate {
        fn drop(&mut self) {
            (self.release)(self.retained)
        }
    }

    #[repr(C)]
    #[derive(Debug)]
    pub struct rtc_http_Request<'a> {
        url: rtc_String<'a>,
        method: i32,
        headers: rtc_http_Headers<'a>,
        body: rtc_Bytes<'a>,
    }

    #[repr(C)]
    #[derive(Debug)]
    pub struct rtc_http_Headers<'a> {
        pub ptr: *const rtc_http_Header<'a>,
        pub count: size_t,
        phantom: std::marker::PhantomData<&'a rtc_http_Header<'a>>,
    }

    impl<'a, T: AsRef<[rtc_http_Header<'a>]>> From<&'a T> for rtc_http_Headers<'a> {
        fn from(headers: &'a T) -> Self {
            let headers = headers.as_ref();
            Self {
                ptr: headers.as_ptr(),
                count: headers.len(),
                phantom: std::marker::PhantomData,
            }
        }
    }

    #[repr(C)]
    #[derive(Debug)]
    pub struct rtc_http_Header<'a> {
        pub name: rtc_String<'a>,
        pub value: rtc_String<'a>,
    }

    #[repr(C)]
    #[derive(Debug)]
    pub struct rtc_http_Response<'a> {
        pub status_code: u16,
        pub body: rtc_Bytes<'a>,
    }

    // Returns an owned pointer which should be destroyed
    // with rtc_http_Client_destroy.
    #[no_mangle]
    pub extern "C" fn rtc_http_Client_create(delegate: rtc_http_Delegate) -> *mut Client {
        Box::into_raw(Box::new(http::DelegatingClient::new(delegate)))
    }

    /// # Safety
    ///
    /// client_ptr must come from rtc_http_Client_create and not already be destroyed
    #[no_mangle]
    pub unsafe extern "C" fn rtc_http_Client_destroy(client_ptr: *mut Client) {
        let client = Box::from_raw(client_ptr);
        drop(client)
    }

    /// # Safety
    ///
    /// client_ptr must come from rtc_http_Client_create and not already be destroyed
    #[no_mangle]
    #[allow(non_snake_case)]
    pub unsafe extern "C" fn rtc_http_Client_received_response(
        client: *const Client,
        request_id: u32,
        response: rtc_http_Response,
    ) {
        info!("rtc_http_Client_received_response():");

        if let Some(client) = client.as_ref() {
            let response = Some(http::Response {
                status: response.status_code.into(),
                body: response.body.to_vec(),
            });
            client.received_response(request_id, response);
        } else {
            error!("Got null ptr in rtc_http_Client_received_response");
        }
    }

    /// # Safety
    ///
    /// client_ptr must come from rtc_http_Client_create and not already be destroyed
    #[no_mangle]
    #[allow(non_snake_case)]
    pub unsafe extern "C" fn rtc_http_Client_request_failed(
        client: *const Client,
        request_id: u32,
    ) {
        info!("rtc_http_Client_request_failed():");

        if let Some(client) = client.as_ref() {
            let response = None;
            client.received_response(request_id, response);
        } else {
            error!("Got null ptr in rtc_http_Client_request_failed");
        }
    }

    impl super::Delegate for rtc_http_Delegate {
        fn send_request(&self, request_id: u32, request: http::Request) {
            info!(
                "rtc_http_Delegate:send_request(): request_id: {}",
                request_id
            );

            let headers: Vec<rtc_http_Header> = request
                .headers
                .iter()
                .map(|(name, value)| rtc_http_Header {
                    name: rtc_String::from(name),
                    value: rtc_String::from(value),
                })
                .collect();

            let unretained = self.retained;
            (self.send_request)(
                unretained,
                request_id,
                rtc_http_Request {
                    method: request.method as i32,
                    url: rtc_String::from(&request.url),
                    headers: rtc_http_Headers::from(&headers),
                    body: rtc_Bytes::from_or_default(request.body.as_ref()),
                },
            );
        }
    }
}

#[cfg(feature = "sim_http")]
pub mod sim {
    use std::{io::Read, sync::Arc};

    use crate::{
        common::actor::{Actor, Stopper},
        lite::http,
    };

    #[derive(Clone)]
    pub struct HttpClient {
        actor: Actor<()>,
    }

    impl HttpClient {
        pub fn start() -> Self {
            rustls::crypto::ring::default_provider()
                .install_default()
                .expect("Failed to install rustls crypto provider");
            Self {
                actor: Actor::start("HttpClient", Stopper::new(), |_| Ok(())).unwrap(),
            }
        }
    }

    impl http::Client for HttpClient {
        fn send_request(&self, request: http::Request, response_callback: http::ResponseCallback) {
            let http::Request {
                method,
                url,
                headers,
                body,
            } = request;

            self.actor.send(move |_| {
                let mut tls_config = rustls::client::ClientConfig::builder()
                    .with_root_certificates(rustls::RootCertStore::empty())
                    .with_no_client_auth();
                tls_config
                    .dangerous()
                    .set_certificate_verifier(Arc::new(ServerCertVerifier::new(
                        rustls::crypto::ring::default_provider(),
                    )));
                let agent = ureq::builder().tls_config(Arc::new(tls_config)).build();

                let mut request = match method {
                    http::Method::Get => agent.get(&url),
                    http::Method::Put => agent.put(&url),
                    http::Method::Delete => agent.delete(&url),
                    http::Method::Post => agent.post(&url),
                };
                for (key, value) in headers.iter() {
                    request = request.set(key, value);
                }
                let request_result = match body {
                    Some(body) => request.send_bytes(&body),
                    None => request.call(),
                };
                match request_result {
                    Ok(response) => {
                        let status_code = response.status();
                        let mut body = Vec::new();
                        if response.into_reader().read_to_end(&mut body).is_ok() {
                            response_callback(Some(http::Response {
                                status: status_code.into(),
                                body,
                            }));
                        } else {
                            response_callback(None);
                        }
                    }
                    Err(ureq::Error::Status(status_code, response)) => {
                        let mut body = Vec::new();
                        if response.into_reader().read_to_end(&mut body).is_ok() {
                            response_callback(Some(http::Response {
                                status: status_code.into(),
                                body,
                            }));
                        } else {
                            response_callback(None);
                        }
                    }
                    Err(ureq::Error::Transport(_)) => {
                        response_callback(None);
                    }
                }
            });
        }
    }

    #[derive(Debug)]
    struct ServerCertVerifier(rustls::crypto::CryptoProvider);

    impl ServerCertVerifier {
        pub fn new(provider: rustls::crypto::CryptoProvider) -> Self {
            Self(provider)
        }
    }

    impl rustls::client::danger::ServerCertVerifier for ServerCertVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }
}
