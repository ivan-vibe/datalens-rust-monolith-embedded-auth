//! The caller's bearer token, scoped to one HTTP request.
//!
//! With `AUTH__TYPE=NATIVE` the server validates the JWT once, then runs the handler inside
//! [`with_token`]. The `us` client and the connection resolver read it with
//! [`current_token`], so every entry fetch is made as the caller rather than with the
//! service's master token. There is no cross-request state: the token lives in a task-local
//! and is gone when the future completes.

use std::future::Future;

tokio::task_local! {
    static REQUEST_TOKEN: Option<String>;
}

/// The bearer token of the request being served, if `AUTH__TYPE=NATIVE` is on.
pub fn current_token() -> Option<String> {
    REQUEST_TOKEN.try_with(Clone::clone).ok().flatten()
}

/// Run `future` with `token` visible to [`current_token`].
pub async fn with_token<F: Future>(token: String, future: F) -> F::Output {
    REQUEST_TOKEN.scope(Some(token), future).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_token_is_visible_only_inside_the_scope() {
        assert_eq!(current_token(), None);
        let seen = with_token("user-jwt".to_string(), async { current_token() }).await;
        assert_eq!(seen.as_deref(), Some("user-jwt"));
        assert_eq!(current_token(), None);
    }
}
