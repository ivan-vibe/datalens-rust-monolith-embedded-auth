//! The authenticated user for the duration of one request.
//!
//! RLS filters are built inside [`crate::compile`] and its siblings, which do not take a
//! request object. The auth middleware installs this task-local once; the compiler reads it
//! when the dataset has RLS rules. There is no global state and no leakage between requests.

use std::future::Future;

/// The subject RLS rules are evaluated for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    /// `dl_auth_native`'s `userId`: compared with `subject_id` and used by `userid: userid`.
    pub user_id: String,
    /// Group ids the caller belongs to (the open-source auth service does not issue groups;
    /// a JWT may carry a `groups` claim).
    pub groups: Vec<String>,
    /// `roles` from the JWT (`datalens.admin`, ...).
    pub roles: Vec<String>,
}

impl Identity {
    pub fn is_admin(&self) -> bool {
        self.roles.iter().any(|role| role == "datalens.admin")
    }
}

tokio::task_local! {
    static IDENTITY: Identity;
}

/// The identity of the request being compiled, if `AUTH__TYPE=NATIVE` installed one.
pub fn current() -> Option<Identity> {
    IDENTITY.try_with(Clone::clone).ok()
}

/// Run `future` with `identity` visible to [`current`].
pub async fn with_identity<F: Future>(identity: Identity, future: F) -> F::Output {
    IDENTITY.scope(identity, future).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_identity_is_scoped_to_the_request() {
        assert!(current().is_none());
        let identity = Identity {
            user_id: "u1".to_string(),
            groups: vec!["eu".to_string()],
            roles: vec!["datalens.admin".to_string()],
        };
        let seen = with_identity(identity.clone(), async { current() }).await;
        assert_eq!(seen, Some(identity));
        assert!(current().is_none());
    }
}
