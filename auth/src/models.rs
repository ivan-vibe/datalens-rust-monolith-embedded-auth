//! Response shapes shared by the auth and user-management routes.

use serde::Serialize;

use crate::ids::encode_id;

/// A user row as the services use it (numeric id, raw role strings).
#[derive(Debug, Clone)]
pub struct UserData {
    pub user_id: i64,
    pub login: Option<String>,
    pub email: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub idp_type: Option<String>,
    pub idp_slug: Option<String>,
    pub roles: Vec<String>,
}

/// `userModel.format` + `roles`: field order matches the Node response exactly.
#[derive(Debug, Serialize)]
pub struct UserOut {
    #[serde(rename = "userId")]
    pub user_id: String,
    pub login: Option<String>,
    pub email: Option<String>,
    #[serde(rename = "firstName")]
    pub first_name: Option<String>,
    #[serde(rename = "lastName")]
    pub last_name: Option<String>,
    #[serde(rename = "idpType")]
    pub idp_type: Option<String>,
    #[serde(rename = "idpSlug")]
    pub idp_slug: Option<String>,
    pub roles: Vec<String>,
}

impl UserOut {
    pub fn from_data(data: &UserData) -> Self {
        Self {
            user_id: encode_id(data.user_id),
            login: data.login.clone(),
            email: data.email.clone(),
            first_name: data.first_name.clone(),
            last_name: data.last_name.clone(),
            idp_type: data.idp_type.clone(),
            idp_slug: data.idp_slug.clone(),
            roles: data.roles.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct UsersListResponse {
    #[serde(rename = "nextPageToken", skip_serializing_if = "Option::is_none")]
    pub next_page_token: Option<String>,
    pub users: Vec<UserOut>,
}

#[derive(Debug, Serialize)]
pub struct UsersByIdsResponse {
    pub users: Vec<UserOut>,
}

#[derive(Debug, Serialize)]
pub struct ProfileResponse {
    pub profile: UserOut,
}

#[derive(Debug, Serialize)]
pub struct CreateUserResponse {
    #[serde(rename = "userId")]
    pub user_id: String,
}
