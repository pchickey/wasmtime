use crate::r#struct::ActiveResponse;
pub use crate::r#struct::WasiHttp;
use crate::types::{RequestOptions, Scheme};
use anyhow::bail;
use bytes::{BufMut, Bytes, BytesMut};
use futures::executor;
use http_body_util::{BodyExt, Full};
use hyper::Method;
use hyper::Request;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio::time::timeout;

impl crate::default_outgoing_http::Host for WasiHttp {
    fn handle(
        &mut self,
        request_id: crate::default_outgoing_http::OutgoingRequest,
        options: Option<crate::default_outgoing_http::RequestOptions>,
    ) -> wasmtime::Result<crate::default_outgoing_http::FutureIncomingResponse> {
        // TODO: Initialize this once?
        let rt = Runtime::new().unwrap();
        let _enter = rt.enter();

        let f = self.handle_async(request_id, options);
        match executor::block_on(f) {
            Ok(r) => {
                println!("{} OK", r);
                Ok(r)
            }
            Err(e) => {
                println!("{} ERR", e);
                Err(e)
            }
        }
    }
}

fn port_for_scheme(scheme: &Option<Scheme>) -> &str {
    match scheme {
        Some(s) => match s {
            Scheme::Http => ":80",
            Scheme::Https => ":443",
            // This should never happen.
            _ => panic!("unsupported scheme!"),
        },
        None => ":443",
    }
}

impl WasiHttp {
    async fn handle_async(
        &mut self,
        request_id: crate::default_outgoing_http::OutgoingRequest,
        options: Option<crate::default_outgoing_http::RequestOptions>,
    ) -> wasmtime::Result<crate::default_outgoing_http::FutureIncomingResponse> {
        let opts = match options {
            Some(o) => o,
            // TODO: Configurable defaults here?
            None => RequestOptions {
                connect_timeout_ms: Some(600 * 1000),
                first_byte_timeout_ms: Some(600 * 1000),
                between_bytes_timeout_ms: Some(600 * 1000),
            },
        };
        let connect_timeout =
            Duration::from_millis(opts.connect_timeout_ms.unwrap_or(600 * 1000).into());
        let first_bytes_timeout =
            Duration::from_millis(opts.first_byte_timeout_ms.unwrap_or(600 * 1000).into());
        let between_bytes_timeout =
            Duration::from_millis(opts.between_bytes_timeout_ms.unwrap_or(600 * 1000).into());

        let request = match self.requests.get(&request_id) {
            Some(r) => r,
            None => bail!("not found!"),
        };

        let method = match request.method {
            crate::types::Method::Get => Method::GET,
            crate::types::Method::Head => Method::HEAD,
            crate::types::Method::Post => Method::POST,
            crate::types::Method::Put => Method::PUT,
            crate::types::Method::Delete => Method::DELETE,
            crate::types::Method::Connect => Method::CONNECT,
            crate::types::Method::Options => Method::OPTIONS,
            crate::types::Method::Trace => Method::TRACE,
            crate::types::Method::Patch => Method::PATCH,
            _ => bail!("unknown method!"),
        };

        let scheme = match request.scheme.as_ref().unwrap_or(&Scheme::Https) {
            Scheme::Http => "http://",
            Scheme::Https => "https://",
            // TODO: this is wrong, fix this.
            _ => panic!("Unsupported scheme!"),
        };

        // Largely adapted from https://hyper.rs/guides/1/client/basic/
        let authority = match request.authority.find(":") {
            Some(_) => request.authority.clone(),
            None => request.authority.clone() + port_for_scheme(&request.scheme),
        };

        let mut sender = if scheme == "https://" {
            let stream = TcpStream::connect(authority.clone()).await?;
            let connector = tokio_native_tls::native_tls::TlsConnector::builder().build()?;
            let connector = tokio_native_tls::TlsConnector::from(connector);
            let host = authority.split(":").next().unwrap_or(&authority);
            let stream = connector.connect(&host, stream).await?;
            let t = timeout(
                connect_timeout,
                hyper::client::conn::http1::handshake(stream),
            )
            .await?;
            let (s, conn) = t?;
            tokio::task::spawn(async move {
                if let Err(err) = conn.await {
                    println!("Connection failed: {:?}", err);
                }
            });
            s
        } else {
            let tcp = TcpStream::connect(authority).await?;
            let t = timeout(connect_timeout, hyper::client::conn::http1::handshake(tcp)).await?;
            let (s, conn) = t?;
            tokio::task::spawn(async move {
                if let Err(err) = conn.await {
                    println!("Connection failed: {:?}", err);
                }
            });
            s
        };

        let url = scheme.to_owned() + &request.authority + &request.path + &request.query;

        let mut call = Request::builder()
            .method(method)
            .uri(url)
            .header(hyper::header::HOST, request.authority.as_str());

        for (key, val) in request.headers.iter() {
            for item in val {
                call = call.header(key, item.clone());
            }
        }

        let response_id = self.response_id_base;
        self.response_id_base = self.response_id_base + 1;
        let mut response = ActiveResponse::new(response_id);
        let body = Full::<Bytes>::new(request.body.clone());
        let t = timeout(first_bytes_timeout, sender.send_request(call.body(body)?)).await?;
        let mut res = t?;
        response.status = res.status().try_into()?;
        for (key, value) in res.headers().iter() {
            let mut vec = std::vec::Vec::new();
            vec.push(value.to_str()?.to_string());
            response
                .response_headers
                .insert(key.as_str().to_string(), vec);
        }
        let mut buf = BytesMut::new();
        while let Some(next) = timeout(between_bytes_timeout, res.frame()).await? {
            let frame = next?;
            if let Some(chunk) = frame.data_ref() {
                buf.put(chunk.clone());
            }
        }
        response.body = buf.freeze();
        self.responses.insert(response_id, response);
        Ok(response_id)
    }
}