//! HTTP handlers for the PEP 503 simple index and file downloads.

use std::sync::Arc;

use anyhow::anyhow;
use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::index::{AppState, normalize, project_of};

/// Encode everything except RFC 3986 unreserved characters.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/simple/", get(root_index))
        .route("/simple/{name}/", get(project_page))
        .route("/simple/{name}", get(project_redirect))
        .route("/files/{*object}", get(file))
        .with_state(state)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn page(title: &str, links: &str) -> Html<String> {
    Html(format!(
        "<!DOCTYPE html>\n<html><head><meta name=\"pypi:repository-version\" content=\"1.0\">\
         <title>{}</title></head><body>\n{links}</body></html>\n",
        esc(title)
    ))
}

fn internal(e: anyhow::Error) -> Response {
    eprintln!("gcs-pypi: error: {e:#}");
    (StatusCode::BAD_GATEWAY, format!("gcs-pypi: {e:#}\n")).into_response()
}

async fn root_index(State(st): State<Arc<AppState>>) -> Response {
    let index = match st.index().await {
        Ok(i) => i,
        Err(e) => return internal(e),
    };
    let links: String = index
        .keys()
        .map(|p| format!("<a href=\"{0}/\">{0}</a><br/>\n", esc(p)))
        .collect();
    page("Simple index", &links).into_response()
}

async fn project_page(State(st): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let project = normalize(&name);
    let index = match st.index().await {
        Ok(i) => i,
        Err(e) => return internal(e),
    };
    let Some(dists) = index.get(&project) else {
        if st.verbose {
            eprintln!("gcs-pypi: {project}: not in bucket");
        }
        return match &st.upstream {
            Some(up) => Redirect::temporary(&format!("{up}{project}/")).into_response(),
            None => (StatusCode::NOT_FOUND, "not found\n").into_response(),
        };
    };
    let links: String = dists
        .iter()
        .map(|d| {
            let path: Vec<String> = d
                .object
                .split('/')
                .map(|seg| utf8_percent_encode(seg, COMPONENT).to_string())
                .collect();
            let frag = d
                .hash
                .as_ref()
                .map(|(alg, hex)| format!("#{alg}={hex}"))
                .unwrap_or_default();
            let rp = d
                .requires_python
                .as_ref()
                .map(|r| format!(" data-requires-python=\"{}\"", esc(r)))
                .unwrap_or_default();
            format!(
                "<a href=\"/files/{}{frag}\"{rp}>{}</a><br/>\n",
                path.join("/"),
                esc(&d.filename)
            )
        })
        .collect();
    page(&format!("Links for {project}"), &links).into_response()
}

async fn project_redirect(Path(name): Path<String>) -> Redirect {
    Redirect::permanent(&format!("/simple/{}/", utf8_percent_encode(&name, COMPONENT)))
}

async fn file(State(st): State<Arc<AppState>>, Path(object): Path<String>) -> Response {
    // Only serve objects that are distributions under the configured prefix.
    let filename = object.rsplit('/').next().unwrap_or(&object);
    if !object.starts_with(&st.prefix) || project_of(filename).is_none() {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    }
    let url = format!(
        "{}/storage/v1/b/{}/o/{}",
        st.endpoint,
        st.bucket,
        utf8_percent_encode(&object, COMPONENT)
    );
    let req = match st.authed(st.http.get(url).query(&[("alt", "media")])).await {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return internal(e.into()),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return internal(anyhow!("download gs://{}/{object}: {status}: {body}", st.bucket));
    }
    if st.verbose {
        eprintln!("gcs-pypi: GET {object}");
    }
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream");
    if let Some(len) = resp.content_length() {
        builder = builder.header(header::CONTENT_LENGTH, len);
    }
    builder
        .body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|e| internal(e.into()))
}
