//! Express compatible default 404 page.
//!
//! Express has no JSON 404 handler: an unmatched request falls through to
//! `finalhandler`, which answers with the well known HTML page
//!
//! ```text
//! <!DOCTYPE html>
//! <html lang="en">
//! <head>
//! <meta charset="utf-8">
//! <title>Error</title>
//! </head>
//! <body>
//! <pre>Cannot GET /v1/entries//meta</pre>
//! </body>
//! </html>
//! ```
//!
//! The message uses `req.method` and the path **without** the query string,
//! HTML-escaped with the rules of the `escape-html` package. The same page is
//! returned both for unknown routes and for known routes whose params do not
//! match (express does not match an empty `:entryId` segment of
//! `/v1/entries//meta`, while axum happily captures it as an empty string).

use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

/// `escape-html`: `&`, `<`, `>`, `"` and `'` are the escaped characters.
fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());

    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }

    escaped
}

/// Body of the express 404 page for `method` + `path`.
pub fn page(method: &Method, path: &str) -> String {
    format!(
        concat!(
            "<!DOCTYPE html>\n",
            "<html lang=\"en\">\n",
            "<head>\n",
            "<meta charset=\"utf-8\">\n",
            "<title>Error</title>\n",
            "</head>\n",
            "<body>\n",
            "<pre>{}</pre>\n",
            "</body>\n",
            "</html>\n",
        ),
        escape_html(&format!("Cannot {} {}", method.as_str(), path)),
    )
}

/// Express style 404 response (`text/html`, connection security headers).
pub fn response(method: &Method, path: &str) -> Response {
    let mut response = (StatusCode::NOT_FOUND, page(method, path)).into_response();
    let headers = response.headers_mut();

    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(
        "Content-Security-Policy",
        HeaderValue::from_static("default-src 'none'"),
    );
    headers.insert(
        "X-Content-Type-Options",
        HeaderValue::from_static("nosniff"),
    );

    response
}

/// `app.use` fallback: the last express layer, matches every request.
pub async fn not_found(method: Method, uri: Uri) -> Response {
    response(&method, uri.path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn page_matches_express_finalhandler() {
        assert_eq!(
            page(&Method::GET, "/v1/entries//meta"),
            concat!(
                "<!DOCTYPE html>\n",
                "<html lang=\"en\">\n",
                "<head>\n",
                "<meta charset=\"utf-8\">\n",
                "<title>Error</title>\n",
                "</head>\n",
                "<body>\n",
                "<pre>Cannot GET /v1/entries//meta</pre>\n",
                "</body>\n",
                "</html>\n",
            ),
        );
    }

    #[tokio::test]
    async fn unknown_route_answers_with_html_page() {
        let uri: Uri = "/nope?x=1".parse().expect("uri");
        let response = not_found(Method::GET, uri).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );

        let bytes = to_bytes(response.into_body(), 8 * 1024)
            .await
            .expect("body");
        let body = String::from_utf8(bytes.to_vec()).expect("utf8");

        // the query string is not part of the message
        assert!(body.contains("<pre>Cannot GET /nope</pre>"), "{body}");
    }

    #[test]
    fn html_is_escaped() {
        assert_eq!(
            escape_html("<script>&\"'"),
            "&lt;script&gt;&amp;&quot;&#39;"
        );
    }
}
