use axum::{
    body::Body,
    extract::Path,
    http::{header, HeaderValue, Response, StatusCode},
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../coolify-ui/dist"]
struct Assets;

pub async fn serve(Path(path): Path<String>) -> Response<Body> {
    serve_path(&path).unwrap_or_else(|| {
        response(
            StatusCode::NOT_FOUND,
            "text/plain",
            "not found".as_bytes().to_vec(),
        )
    })
}

pub async fn fallback() -> Response<Body> {
    serve_path("index.html").unwrap_or_else(|| {
        response(
            StatusCode::OK,
            "text/html; charset=utf-8",
            b"Coolify v5".to_vec(),
        )
    })
}

fn serve_path(path: &str) -> Option<Response<Body>> {
    let path = path.trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let asset = Assets::get(path).or_else(|| Assets::get("index.html"))?;
    let mime = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();
    Some(response(StatusCode::OK, &mime, asset.data.into_owned()))
}

fn response(status: StatusCode, content_type: &str, body: Vec<u8>) -> Response<Body> {
    let mut res = Response::new(Body::from(body));
    *res.status_mut() = status;
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    res
}
