//! zod-equivalent validation for the auth and user-management endpoints.
//!
//! The issue objects (codes, field names, messages, paths, option orders) match the Node
//! service's zod responses closely enough that the differential suite compares them exactly
//! for the cases it exercises. Unknown object keys are stripped, like `z.object` (not strict).

use serde_json::{json, Map, Value};

use crate::ids::{decode_id_lenient, LenientIdError};

pub const ROLE_EDITOR: &str = "datalens.editor";
pub const ROLE_ADMIN: &str = "datalens.admin";
pub const ROLE_VIEWER: &str = "datalens.viewer";
pub const ROLE_VISITOR: &str = "datalens.visitor";
pub const ROLE_CREATOR: &str = "datalens.creator";

/// `Object.values(UserRole)` order, which is the `options` order in zod enum issues.
pub const ROLES: &[&str] = &[ROLE_EDITOR, ROLE_ADMIN, ROLE_VIEWER, ROLE_VISITOR, ROLE_CREATOR];

pub const LOGIN_REGEX_ERROR_MESSAGE: &str =
    "Login should starts with [a-zA-Z] character and may contain number and special character [_-]";
pub const PASSWORD_REGEX_ERROR_MESSAGE: &str = "Password should be >= 8 chars with uppercase [A-Z] and lowercase [a-z] character, number and special character [!@#$%^&*_-]";

pub fn is_known_role(role: &str) -> bool {
    ROLES.contains(&role)
}

/// zod paths use numbers for array indices: `["roles", 0]`, not `["roles", "0"]`.
fn path_value(path: &[&str]) -> Value {
    Value::Array(
        path.iter()
            .map(|part| match part.parse::<u64>() {
                Ok(index) if part.chars().all(|ch| ch.is_ascii_digit()) => {
                    Value::Number(index.into())
                }
                _ => Value::String((*part).to_string()),
            })
            .collect(),
    )
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

pub fn invalid_type_issue(expected: &str, received: &str, path: &[&str]) -> Value {
    let message = if received == "undefined" {
        "Required".to_string()
    } else {
        format!("Expected {expected}, received {received}")
    };
    json!({
        "code": "invalid_type",
        "expected": expected,
        "received": received,
        "path": path_value(path),
        "message": message,
    })
}

fn custom_issue(message: &str, path: &[&str], fatal: bool) -> Value {
    if fatal {
        json!({"code": "custom", "message": message, "fatal": true, "path": path_value(path)})
    } else {
        json!({"code": "custom", "message": message, "path": path_value(path)})
    }
}

fn number_bound_issue(code: &str, key: &str, bound: i64, path: &[&str]) -> Value {
    let comparator = if code == "too_small" {
        "greater than or equal to"
    } else {
        "less than or equal to"
    };
    json!({
        "code": code,
        key: bound,
        "type": "number",
        "inclusive": true,
        "exact": false,
        "message": format!("Number must be {comparator} {bound}"),
        "path": path_value(path),
    })
}

fn string_bound_issue(code: &str, bound: usize, path: &[&str]) -> Value {
    let message = if code == "too_small" {
        format!("String must contain at least {bound} character(s)")
    } else {
        format!("String must contain at most {bound} character(s)")
    };
    json!({
        "code": code,
        if code == "too_small" { "minimum" } else { "maximum" }: bound,
        "type": "string",
        "inclusive": true,
        "exact": false,
        "message": message,
        "path": path_value(path),
    })
}

fn array_bound_issue(code: &str, bound: usize, path: &[&str]) -> Value {
    let message = if code == "too_small" {
        format!("Array must contain at least {bound} element(s)")
    } else {
        format!("Array must contain at most {bound} element(s)")
    };
    json!({
        "code": code,
        if code == "too_small" { "minimum" } else { "maximum" }: bound,
        "type": "array",
        "inclusive": true,
        "exact": false,
        "message": message,
        "path": path_value(path),
    })
}

fn invalid_enum_issue(received: &str, path: &[&str]) -> Value {
    let expected = ROLES
        .iter()
        .map(|role| format!("'{role}'"))
        .collect::<Vec<_>>()
        .join(" | ");
    json!({
        "received": received,
        "code": "invalid_enum_value",
        "options": ROLES,
        "path": path_value(path),
        "message": format!("Invalid enum value. Expected {expected}, received '{received}'"),
    })
}

fn invalid_email_issue(path: &[&str]) -> Value {
    json!({"validation": "email", "code": "invalid_string", "message": "Invalid email", "path": path})
}

fn password_regex_issue(path: &[&str]) -> Value {
    json!({
        "validation": "regex",
        "code": "invalid_string",
        "message": PASSWORD_REGEX_ERROR_MESSAGE,
        "path": path_value(path),
    })
}

pub fn email_valid(value: &str) -> bool {
    // Approximation of zod's email check: one `@`, non-empty local part, domain with a dot,
    // no whitespace. Real login emails in this deployment are ASCII.
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !local.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !value.chars().any(char::is_whitespace)
}

fn login_regex_ok(login: &str) -> bool {
    let bytes = login.as_bytes();
    matches!(bytes.first(), Some(byte) if byte.is_ascii_alphabetic())
        && matches!(bytes.last(), Some(byte) if byte.is_ascii_alphanumeric())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

pub fn login_issues(login: &str) -> Vec<Value> {
    let length = login.chars().count();
    if length < 3 {
        return vec![custom_issue("Login should contain at least 3 characters", &["login"], true)];
    }
    if length > 200 {
        return vec![custom_issue("Login should contain at most 200 characters", &["login"], true)];
    }
    if login.contains('@') {
        if !email_valid(login) {
            return vec![custom_issue(
                "Login with '@' character should be valid email",
                &["login"],
                true,
            )];
        }
        return Vec::new();
    }
    if !login_regex_ok(login) {
        return vec![custom_issue(LOGIN_REGEX_ERROR_MESSAGE, &["login"], true)];
    }
    Vec::new()
}

pub fn password_issues(password: &str, path: &[&str]) -> Vec<Value> {
    let mut issues = Vec::new();
    if password.chars().count() < 8 {
        issues.push(string_bound_issue("too_small", 8, path));
    }
    let valid = password.chars().count() >= 8
        && password.chars().any(|ch| ch.is_ascii_uppercase())
        && password.chars().any(|ch| ch.is_ascii_lowercase())
        && password.chars().any(|ch| ch.is_ascii_digit())
        && password.chars().any(|ch| "!@#$%^&*_-".contains(ch));
    if !valid {
        issues.push(password_regex_issue(path));
    }
    issues
}

fn object_body(body: Option<&Value>) -> Result<&Map<String, Value>, Vec<Value>> {
    match body {
        None => Err(vec![invalid_type_issue("object", "undefined", &[])]),
        Some(Value::Object(map)) => Ok(map),
        Some(other) => Err(vec![invalid_type_issue("object", json_type(other), &[])]),
    }
}

// ---------------------------------------------------------------------------------------
// GET /v1/users/list
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum IdpType {
    Null,
    Value(String),
}

#[derive(Debug, Clone)]
pub struct ListQuery {
    pub page: i64,
    pub page_size: i64,
    pub filter_string: Option<String>,
    pub idp_type: Option<IdpType>,
    pub roles: Option<Vec<String>>,
}

/// qs-compatible query parsing: `a=1&a=2` keeps both values, `+` decodes to a space.
pub fn parse_raw_query(raw: Option<&str>) -> Vec<(String, String)> {
    let Some(raw) = raw else { return Vec::new() };
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode_form(key), decode_form(value))
        })
        .collect()
}

fn decode_form(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
                match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                    Some(value) => {
                        out.push(value);
                        index += 3;
                    }
                    None => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `parseInt(value, 10)` from JavaScript for the forms the auth routes accept.
fn js_parse_int(raw: &str) -> Option<i64> {
    let trimmed = raw.trim_start();
    let mut chars = trimmed.chars().peekable();
    let mut sign = 1i64;
    if let Some(ch) = chars.peek() {
        if *ch == '-' || *ch == '+' {
            if *ch == '-' {
                sign = -1;
            }
            chars.next();
        }
    }
    let mut value: i128 = 0;
    let mut digits = 0;
    for ch in chars {
        let Some(digit) = ch.to_digit(10) else { break };
        value = (value * 10 + digit as i128).min(i64::MAX as i128 + 1);
        digits += 1;
    }
    if digits == 0 {
        return None;
    }
    let value = value * sign as i128;
    if value > i64::MAX as i128 {
        Some(i64::MAX)
    } else if value < i64::MIN as i128 {
        Some(i64::MIN)
    } else {
        Some(value as i64)
    }
}

fn scalar_param(params: &[(String, String)], name: &str) -> Result<Option<String>, Value> {
    let values: Vec<&String> = params
        .iter()
        .filter(|(key, _)| key == name)
        .map(|(_, value)| value)
        .collect();
    match values.len() {
        0 => Ok(None),
        1 => Ok(Some(values[0].clone())),
        _ => Err(invalid_type_issue("string", "array", &[name])),
    }
}

fn validate_scalar_string(
    raw: &Option<String>,
    name: &str,
    min: usize,
    max: usize,
    issues: &mut Vec<Value>,
) -> Option<String> {
    let value = raw.clone()?;
    let length = value.chars().count();
    if length < min {
        issues.push(string_bound_issue("too_small", min, &[name]));
    }
    if length > max {
        issues.push(string_bound_issue("too_big", max, &[name]));
    }
    Some(value)
}

pub fn validate_list_query(raw: Option<&str>) -> Result<ListQuery, Vec<Value>> {
    let params = parse_raw_query(raw);
    let mut issues = Vec::new();

    // page
    let mut page = 0i64;
    match scalar_param(&params, "page") {
        Err(issue) => issues.push(issue),
        Ok(Some(raw)) => match js_parse_int(&raw) {
            None => issues.push(invalid_type_issue("number", "nan", &["page"])),
            Some(value) if value < 0 => {
                issues.push(number_bound_issue("too_small", "minimum", 0, &["page"]))
            }
            Some(value) => page = value,
        },
        Ok(None) => {}
    }

    // pageSize
    let mut page_size = 20i64;
    match scalar_param(&params, "pageSize") {
        Err(issue) => issues.push(issue),
        Ok(Some(raw)) => match js_parse_int(&raw) {
            None => issues.push(invalid_type_issue("number", "nan", &["pageSize"])),
            Some(value) if value < 1 => {
                issues.push(number_bound_issue("too_small", "minimum", 1, &["pageSize"]))
            }
            Some(value) if value > 100 => {
                issues.push(number_bound_issue("too_big", "maximum", 100, &["pageSize"]))
            }
            Some(value) => page_size = value,
        },
        Ok(None) => {}
    }

    let filter_string = match scalar_param(&params, "filterString") {
        Err(issue) => {
            issues.push(issue);
            None
        }
        Ok(raw) => validate_scalar_string(&raw, "filterString", 1, 100, &mut issues),
    };
    let idp_raw = match scalar_param(&params, "idpType") {
        Err(issue) => {
            issues.push(issue);
            None
        }
        Ok(raw) => validate_scalar_string(&raw, "idpType", 1, 100, &mut issues),
    };

    let role_values: Vec<(usize, String)> = params
        .iter()
        .filter(|(key, _)| key == "roles")
        .map(|(_, value)| value.clone())
        .enumerate()
        .collect();
    let mut roles = None;
    if !role_values.is_empty() {
        let mut parsed = Vec::new();
        for (index, role) in &role_values {
            if is_known_role(role) {
                parsed.push(role.clone());
            } else {
                let index = index.to_string();
                issues.push(invalid_enum_issue(role, &["roles", &index]));
            }
        }
        if role_values.len() > 10 {
            issues.push(array_bound_issue("too_big", 10, &["roles"]));
        }
        roles = Some(parsed);
    }

    if !issues.is_empty() {
        return Err(issues);
    }
    Ok(ListQuery {
        page,
        page_size,
        filter_string,
        idp_type: idp_raw.map(|raw| {
            if raw == "null" {
                IdpType::Null
            } else {
                IdpType::Value(raw)
            }
        }),
        roles,
    })
}

// ---------------------------------------------------------------------------------------
// POST /v1/users/get-by-ids
// ---------------------------------------------------------------------------------------

#[derive(Debug)]
pub enum SubjectIdsError {
    Validation(Vec<Value>),
    NumericOverflow,
}

pub fn validate_subject_ids(body: Option<&Value>) -> Result<Vec<i64>, SubjectIdsError> {
    let map = object_body(body).map_err(SubjectIdsError::Validation)?;
    let Some(subject_ids) = map.get("subjectIds") else {
        return Err(SubjectIdsError::Validation(vec![invalid_type_issue(
            "array",
            "undefined",
            &["subjectIds"],
        )]));
    };
    let Value::Array(values) = subject_ids else {
        return Err(SubjectIdsError::Validation(vec![invalid_type_issue(
            "array",
            json_type(subject_ids),
            &["subjectIds"],
        )]));
    };
    let mut issues = Vec::new();
    if values.is_empty() {
        issues.push(array_bound_issue("too_small", 1, &["subjectIds"]));
    }
    if values.len() > 1000 {
        issues.push(array_bound_issue("too_big", 1000, &["subjectIds"]));
    }
    let mut decoded = Vec::new();
    for (index, value) in values.iter().enumerate() {
        let Value::String(raw) = value else {
            let index = index.to_string();
            issues.push(invalid_type_issue(
                "string",
                json_type(value),
                &["subjectIds", &index],
            ));
            continue;
        };
        match decode_id_lenient(raw) {
            Ok(id) => decoded.push(id),
            Err(LenientIdError::InvalidChar) => issues.push(custom_issue(
                &format!("id '{raw}' has incorrect format"),
                &["subjectIds"],
                false,
            )),
            Err(LenientIdError::Overflow) => return Err(SubjectIdsError::NumericOverflow),
        }
    }
    if !issues.is_empty() {
        return Err(SubjectIdsError::Validation(issues));
    }
    Ok(decoded)
}

// ---------------------------------------------------------------------------------------
// profile / password bodies
// ---------------------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub enum UpdateField {
    #[default]
    Absent,
    Null,
    Value(String),
}

#[derive(Debug, Default, Clone)]
pub struct ProfileInput {
    pub email: UpdateField,
    pub first_name: UpdateField,
    pub last_name: UpdateField,
}

fn nullable_string_field(
    map: &Map<String, Value>,
    key: &str,
    min: usize,
    max: usize,
    issues: &mut Vec<Value>,
) -> UpdateField {
    let Some(value) = map.get(key) else {
        return UpdateField::Absent;
    };
    match value {
        Value::Null => UpdateField::Null,
        Value::String(text) => {
            let length = text.chars().count();
            if length < min {
                issues.push(string_bound_issue("too_small", min, &[key]));
            }
            if length > max {
                issues.push(string_bound_issue("too_big", max, &[key]));
            }
            UpdateField::Value(text.clone())
        }
        other => {
            issues.push(invalid_type_issue("string", json_type(other), &[key]));
            UpdateField::Absent
        }
    }
}

pub fn validate_profile_body(body: Option<&Value>) -> Result<ProfileInput, Vec<Value>> {
    let map = object_body(body)?;
    let mut issues = Vec::new();
    let email = match map.get("email") {
        None => UpdateField::Absent,
        Some(Value::Null) => UpdateField::Null,
        Some(Value::String(text)) if email_valid(text) => UpdateField::Value(text.clone()),
        Some(Value::String(_)) => {
            issues.push(invalid_email_issue(&["email"]));
            UpdateField::Absent
        }
        Some(other) => {
            issues.push(invalid_type_issue("string", json_type(other), &["email"]));
            UpdateField::Absent
        }
    };
    let first_name = nullable_string_field(map, "firstName", 1, 200, &mut issues);
    let last_name = nullable_string_field(map, "lastName", 1, 200, &mut issues);
    if !issues.is_empty() {
        return Err(issues);
    }
    Ok(ProfileInput {
        email,
        first_name,
        last_name,
    })
}

#[derive(Debug)]
pub struct MePasswordInput {
    pub new_password: String,
    pub old_password: String,
}

pub fn validate_me_password_body(body: Option<&Value>) -> Result<MePasswordInput, Vec<Value>> {
    let map = object_body(body)?;
    let mut issues = Vec::new();

    let new_password = match map.get("newPassword") {
        None => {
            issues.push(invalid_type_issue("string", "undefined", &["newPassword"]));
            String::new()
        }
        Some(Value::String(password)) => {
            issues.extend(password_issues(password, &["newPassword"]));
            password.clone()
        }
        Some(other) => {
            issues.push(invalid_type_issue("string", json_type(other), &["newPassword"]));
            String::new()
        }
    };
    let old_password = match map.get("oldPassword") {
        None => {
            issues.push(invalid_type_issue("string", "undefined", &["oldPassword"]));
            String::new()
        }
        Some(Value::String(password)) => {
            if password.is_empty() {
                issues.push(string_bound_issue("too_small", 1, &["oldPassword"]));
            }
            password.clone()
        }
        Some(other) => {
            issues.push(invalid_type_issue("string", json_type(other), &["oldPassword"]));
            String::new()
        }
    };
    if !issues.is_empty() {
        return Err(issues);
    }
    Ok(MePasswordInput {
        new_password,
        old_password,
    })
}

#[derive(Debug)]
pub struct AdminPasswordInput {
    pub new_password: String,
}

pub fn validate_admin_password_body(body: Option<&Value>) -> Result<AdminPasswordInput, Vec<Value>> {
    let map = object_body(body)?;
    let mut issues = Vec::new();
    let new_password = match map.get("newPassword") {
        None => {
            issues.push(invalid_type_issue("string", "undefined", &["newPassword"]));
            String::new()
        }
        Some(Value::String(password)) => {
            issues.extend(password_issues(password, &["newPassword"]));
            password.clone()
        }
        Some(other) => {
            issues.push(invalid_type_issue("string", json_type(other), &["newPassword"]));
            String::new()
        }
    };
    if !issues.is_empty() {
        return Err(issues);
    }
    Ok(AdminPasswordInput { new_password })
}

// ---------------------------------------------------------------------------------------
// POST /v1/management/users/create
// ---------------------------------------------------------------------------------------

#[derive(Debug)]
pub struct CreateUserInput {
    pub login: String,
    pub password: String,
    pub email: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub roles: Option<Vec<String>>,
}

pub fn validate_create_user_body(body: Option<&Value>) -> Result<CreateUserInput, Vec<Value>> {
    let map = object_body(body)?;
    let mut issues = Vec::new();

    // `login` is `z.string().superRefine(...)`: a wrong type is reported at the schema position,
    // while the refinement's custom issue is appended after all other fields (zod behaviour).
    let mut login: Option<String> = None;
    match map.get("login") {
        None => issues.push(invalid_type_issue("string", "undefined", &["login"])),
        Some(Value::String(text)) => login = Some(text.clone()),
        Some(other) => issues.push(invalid_type_issue("string", json_type(other), &["login"])),
    }
    let password = match map.get("password") {
        None => {
            issues.push(invalid_type_issue("string", "undefined", &["password"]));
            String::new()
        }
        Some(Value::String(password)) => {
            issues.extend(password_issues(password, &["password"]));
            password.clone()
        }
        Some(other) => {
            issues.push(invalid_type_issue("string", json_type(other), &["password"]));
            String::new()
        }
    };

    let email = match map.get("email") {
        None => None,
        Some(Value::String(text)) if email_valid(text) => Some(text.clone()),
        Some(Value::String(_)) => {
            issues.push(invalid_email_issue(&["email"]));
            None
        }
        Some(other) => {
            issues.push(invalid_type_issue("string", json_type(other), &["email"]));
            None
        }
    };

    let mut optional_string = |key: &str| -> Option<String> {
        match map.get(key) {
            None => None,
            Some(Value::String(text)) => {
                let length = text.chars().count();
                if length < 1 {
                    issues.push(string_bound_issue("too_small", 1, &[key]));
                }
                if length > 200 {
                    issues.push(string_bound_issue("too_big", 200, &[key]));
                }
                Some(text.clone())
            }
            Some(other) => {
                issues.push(invalid_type_issue("string", json_type(other), &[key]));
                None
            }
        }
    };
    let first_name = optional_string("firstName");
    let last_name = optional_string("lastName");

    let roles = match map.get("roles") {
        None => None,
        Some(Value::Array(values)) => {
            let mut parsed = Vec::new();
            if values.is_empty() {
                issues.push(array_bound_issue("too_small", 1, &["roles"]));
            }
            if values.len() > 100 {
                issues.push(array_bound_issue("too_big", 100, &["roles"]));
            }
            for (index, value) in values.iter().enumerate() {
                let index = index.to_string();
                match value {
                    Value::String(role) if is_known_role(role) => parsed.push(role.clone()),
                    Value::String(role) => {
                        issues.push(invalid_enum_issue(role, &["roles", &index]))
                    }
                    other => issues.push(invalid_type_issue(
                        "string",
                        json_type(other),
                        &["roles", &index],
                    )),
                }
            }
            Some(parsed)
        }
        Some(other) => {
            issues.push(invalid_type_issue("array", json_type(other), &["roles"]));
            None
        }
    };

    if let Some(login) = &login {
        issues.extend(login_issues(login));
    }
    if !issues.is_empty() {
        return Err(issues);
    }
    Ok(CreateUserInput {
        login: login.unwrap_or_default(),
        password,
        email,
        first_name,
        last_name,
        roles,
    })
}

// ---------------------------------------------------------------------------------------
// role deltas (add / update / remove)
// ---------------------------------------------------------------------------------------

#[derive(Debug)]
pub enum DeltasError {
    Validation(Vec<Value>),
    NumericOverflow,
}

fn deltas_array<'a>(map: &'a Map<String, Value>, issues: &mut Vec<Value>) -> Option<&'a Vec<Value>> {
    match map.get("deltas") {
        None => {
            issues.push(invalid_type_issue("array", "undefined", &["deltas"]));
            None
        }
        Some(Value::Array(values)) => {
            if values.is_empty() {
                issues.push(array_bound_issue("too_small", 1, &["deltas"]));
            }
            if values.len() > 50 {
                issues.push(array_bound_issue("too_big", 50, &["deltas"]));
            }
            Some(values)
        }
        Some(other) => {
            issues.push(invalid_type_issue("array", json_type(other), &["deltas"]));
            None
        }
    }
}

fn delta_field<'a>(
    value: &'a Map<String, Value>,
    key: &str,
    index: &str,
    issues: &mut Vec<Value>,
) -> Option<&'a Value> {
    match value.get(key) {
        Some(field) => Some(field),
        None => {
            issues.push(invalid_type_issue(
                "string",
                "undefined",
                &["deltas", index, key],
            ));
            None
        }
    }
}

fn delta_role(value: Option<&Value>, key: &str, index: &str, issues: &mut Vec<Value>) -> Option<String> {
    let value = value?;
    match value {
        Value::String(role) if is_known_role(role) => Some(role.clone()),
        Value::String(role) => {
            issues.push(invalid_enum_issue(role, &["deltas", index, key]));
            None
        }
        other => {
            issues.push(invalid_type_issue(
                "string",
                json_type(other),
                &["deltas", index, key],
            ));
            None
        }
    }
}

fn delta_subject_id(value: Option<&Value>, index: &str, issues: &mut Vec<Value>) -> Option<i64> {
    let value = value?;
    match value {
        Value::String(raw) => match decode_id_lenient(raw) {
            Ok(id) => Some(id),
            Err(LenientIdError::InvalidChar) => {
                issues.push(custom_issue(
                    &format!("id '{raw}' has incorrect format"),
                    &["deltas"],
                    false,
                ));
                None
            }
            Err(LenientIdError::Overflow) => None,
        },
        other => {
            issues.push(invalid_type_issue(
                "string",
                json_type(other),
                &["deltas", index, "subjectId"],
            ));
            None
        }
    }
}

/// `{role, subjectId}[]` for roles/add and roles/remove.
pub fn validate_add_remove_deltas(body: Option<&Value>) -> Result<Vec<(String, i64)>, DeltasError> {
    let map = object_body(body).map_err(DeltasError::Validation)?;
    let mut issues = Vec::new();
    let Some(values) = deltas_array(map, &mut issues) else {
        return Err(DeltasError::Validation(issues));
    };
    let mut deltas = Vec::new();
    let mut overflow = false;
    for (index, value) in values.iter().enumerate() {
        let index = index.to_string();
        let Value::Object(delta) = value else {
            issues.push(invalid_type_issue(
                "object",
                json_type(value),
                &["deltas", &index],
            ));
            continue;
        };
        let role = delta_role(delta_field(delta, "role", &index, &mut issues), "role", &index, &mut issues);
        let subject = delta_subject_id(
            delta_field(delta, "subjectId", &index, &mut issues),
            &index,
            &mut issues,
        );
        if matches!(delta.get("subjectId"), Some(Value::String(raw)) if decode_id_lenient(raw) == Err(LenientIdError::Overflow)) {
            overflow = true;
        }
        if let (Some(role), Some(subject)) = (role, subject) {
            deltas.push((role, subject));
        }
    }
    if overflow {
        return Err(DeltasError::NumericOverflow);
    }
    if !issues.is_empty() {
        return Err(DeltasError::Validation(issues));
    }
    Ok(deltas)
}

/// `{oldRole, newRole, subjectId}[]` for roles/update.
pub fn validate_update_deltas(body: Option<&Value>) -> Result<Vec<(String, String, i64)>, DeltasError> {
    let map = object_body(body).map_err(DeltasError::Validation)?;
    let mut issues = Vec::new();
    let Some(values) = deltas_array(map, &mut issues) else {
        return Err(DeltasError::Validation(issues));
    };
    let mut deltas = Vec::new();
    let mut overflow = false;
    for (index, value) in values.iter().enumerate() {
        let index = index.to_string();
        let Value::Object(delta) = value else {
            issues.push(invalid_type_issue(
                "object",
                json_type(value),
                &["deltas", &index],
            ));
            continue;
        };
        let old_role = delta_role(
            delta_field(delta, "oldRole", &index, &mut issues),
            "oldRole",
            &index,
            &mut issues,
        );
        let new_role = delta_role(
            delta_field(delta, "newRole", &index, &mut issues),
            "newRole",
            &index,
            &mut issues,
        );
        let subject = delta_subject_id(
            delta_field(delta, "subjectId", &index, &mut issues),
            &index,
            &mut issues,
        );
        if matches!(delta.get("subjectId"), Some(Value::String(raw)) if decode_id_lenient(raw) == Err(LenientIdError::Overflow)) {
            overflow = true;
        }
        if let (Some(old_role), Some(new_role), Some(subject)) = (old_role, new_role, subject) {
            deltas.push((old_role, new_role, subject));
        }
    }
    if overflow {
        return Err(DeltasError::NumericOverflow);
    }
    if !issues.is_empty() {
        return Err(DeltasError::Validation(issues));
    }
    Ok(deltas)
}

// ---------------------------------------------------------------------------------------
// path ids
// ---------------------------------------------------------------------------------------

#[derive(Debug)]
pub enum PathIdError {
    Validation(Vec<Value>),
    NumericOverflow,
}

pub fn decode_path_id(raw: &str) -> Result<i64, PathIdError> {
    decode_id_lenient(raw).map_err(|error| match error {
        LenientIdError::InvalidChar => PathIdError::Validation(vec![custom_issue(
            &format!("id '{raw}' has incorrect format"),
            &["userId"],
            false,
        )]),
        LenientIdError::Overflow => PathIdError::NumericOverflow,
    })
}

// ---------------------------------------------------------------------------------------
// signup body (kept here so login/password rules have one implementation)
// ---------------------------------------------------------------------------------------

pub fn validate_signup_body(body: Option<&Value>) -> Result<(), Vec<Value>> {
    let map = object_body(body)?;
    let mut issues = Vec::new();
    let mut login: Option<String> = None;
    match map.get("login") {
        None => issues.push(invalid_type_issue("string", "undefined", &["login"])),
        Some(Value::String(text)) => login = Some(text.clone()),
        Some(other) => issues.push(invalid_type_issue("string", json_type(other), &["login"])),
    }
    match map.get("password") {
        None => issues.push(invalid_type_issue("string", "undefined", &["password"])),
        Some(Value::String(password)) => issues.extend(password_issues(password, &["password"])),
        Some(other) => issues.push(invalid_type_issue("string", json_type(other), &["password"])),
    }
    if let Some(email) = map.get("email") {
        match email {
            Value::String(text) if email_valid(text) => {}
            Value::String(_) => issues.push(invalid_email_issue(&["email"])),
            other => issues.push(invalid_type_issue("string", json_type(other), &["email"])),
        }
    }
    for (key, min, max) in [("firstName", 1usize, 200usize), ("lastName", 1, 200)] {
        if let Some(value) = map.get(key) {
            match value {
                Value::String(text) => {
                    let length = text.chars().count();
                    if length < min {
                        issues.push(string_bound_issue("too_small", min, &[key]));
                    }
                    if length > max {
                        issues.push(string_bound_issue("too_big", max, &[key]));
                    }
                }
                other => issues.push(invalid_type_issue("string", json_type(other), &[key])),
            }
        }
    }
    if let Some(login) = &login {
        issues.extend(login_issues(login));
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_rules_match_reference() {
        assert!(login_issues("admin").is_empty());
        assert!(login_issues("a_b-c1").is_empty());
        assert!(login_issues("user@example.com").is_empty());
        assert_eq!(login_issues("ab")[0]["message"], "Login should contain at least 3 characters");
        assert_eq!(login_issues("_admin")[0]["message"], LOGIN_REGEX_ERROR_MESSAGE);
        assert_eq!(login_issues("admin_")[0]["message"], LOGIN_REGEX_ERROR_MESSAGE);
        assert_eq!(
            login_issues("user@bad")[0]["message"],
            "Login with '@' character should be valid email"
        );
    }

    #[test]
    fn password_rules_match_reference() {
        assert!(password_issues("Abcdef1!", &["password"]).is_empty());
        assert_eq!(password_issues("Ab1!", &["password"]).len(), 2);
        let issues = password_issues("Abcdefg1", &["password"]);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0]["message"], PASSWORD_REGEX_ERROR_MESSAGE);
    }

    #[test]
    fn list_query_validation_matches_reference_issues() {
        let query = validate_list_query(Some("page=x")).unwrap_err();
        assert_eq!(
            query[0],
            json!({
                "code": "invalid_type",
                "expected": "number",
                "received": "nan",
                "path": ["page"],
                "message": "Expected number, received nan",
            })
        );
        let query = validate_list_query(Some("pageSize=0")).unwrap_err();
        assert_eq!(query[0]["type"], "number");
        assert_eq!(query[0]["minimum"], 1);
        let query = validate_list_query(Some("filterString=")).unwrap_err();
        assert_eq!(query[0]["type"], "string");
        assert_eq!(query[0]["minimum"], 1);
        let query = validate_list_query(Some("roles=bogus&roles=datalens.admin")).unwrap_err();
        assert_eq!(query[0]["path"], json!(["roles", 0]));
        assert_eq!(query[0]["received"], "bogus");
        assert_eq!(query[0]["options"], json!(ROLES));

        let parsed = validate_list_query(Some("page=0&pageSize=5&roles=datalens.admin&idpType=null")).unwrap();
        assert_eq!(parsed.page, 0);
        assert_eq!(parsed.page_size, 5);
        assert!(matches!(parsed.idp_type, Some(IdpType::Null)));
        assert_eq!(parsed.roles.as_deref(), Some(&["datalens.admin".to_string()][..]));
    }

    #[test]
    fn subject_ids_validation_matches_reference_issues() {
        let issue = match validate_subject_ids(Some(&json!({}))) {
            Err(SubjectIdsError::Validation(issues)) => issues,
            _ => panic!("expected validation error"),
        };
        assert_eq!(issue[0]["expected"], "array");
        assert_eq!(issue[0]["received"], "undefined");
        assert_eq!(issue[0]["path"], json!(["subjectIds"]));

        let issue = match validate_subject_ids(Some(&json!({"subjectIds": []}))) {
            Err(SubjectIdsError::Validation(issues)) => issues,
            _ => panic!("expected validation error"),
        };
        assert_eq!(issue[0]["code"], "too_small");
        assert_eq!(issue[0]["type"], "array");

        let issue = match validate_subject_ids(Some(&json!({"subjectIds": [123]}))) {
            Err(SubjectIdsError::Validation(issues)) => issues,
            _ => panic!("expected validation error"),
        };
        assert_eq!(issue[0]["path"], json!(["subjectIds", 0]));
        assert_eq!(issue[0]["expected"], "string");

        let issue = match validate_subject_ids(Some(&json!({"subjectIds": ["12$"]}))) {
            Err(SubjectIdsError::Validation(issues)) => issues,
            _ => panic!("expected validation error"),
        };
        assert_eq!(issue[0]["message"], "id '12$' has incorrect format");
        assert_eq!(issue[0]["path"], json!(["subjectIds"]));

        let ids = validate_subject_ids(Some(&json!({"subjectIds": ["123", "123"]}))).unwrap();
        assert_eq!(ids, vec![1259, 1259]);
    }

    #[test]
    fn profile_body_allows_null_but_validates_strings() {
        let parsed = validate_profile_body(Some(&json!({"email": null, "firstName": "X"}))).unwrap();
        assert!(matches!(parsed.email, UpdateField::Null));
        assert!(matches!(parsed.first_name, UpdateField::Value(ref value) if value == "X"));
        assert!(matches!(parsed.last_name, UpdateField::Absent));

        let issues = validate_profile_body(Some(&json!({"firstName": ""}))).unwrap_err();
        assert_eq!(issues[0]["code"], "too_small");
        let issues = validate_profile_body(Some(&json!({"email": "bad"}))).unwrap_err();
        assert_eq!(issues[0]["code"], "invalid_string");
        assert_eq!(issues[0]["validation"], "email");
    }

    #[test]
    fn create_user_and_delta_validation() {
        let issues = match validate_create_user_body(Some(&json!({"login": "x", "password": "y"}))) {
            Err(issues) => issues,
            Ok(_) => panic!("expected validation error"),
        };
        assert!(issues.iter().any(|issue| issue["path"][0] == "login"));
        assert!(issues.iter().any(|issue| issue["path"][0] == "password"));

        let issues = match validate_create_user_body(Some(&json!({
            "login": "valid-login",
            "password": "Abcdef1!",
            "roles": ["bogus"]
        }))) {
            Err(issues) => issues,
            Ok(_) => panic!("expected validation error"),
        };
        assert_eq!(issues[0]["code"], "invalid_enum_value");
        assert_eq!(issues[0]["path"], json!(["roles", 0]));

        let issues = match validate_add_remove_deltas(Some(&json!({"deltas": []}))) {
            Err(DeltasError::Validation(issues)) => issues,
            _ => panic!("expected validation error"),
        };
        assert_eq!(issues[0]["type"], "array");

        let deltas = validate_add_remove_deltas(Some(&json!({
            "deltas": [{"role": "datalens.viewer", "subjectId": "123"}]
        })))
        .expect("deltas");
        assert_eq!(deltas, vec![("datalens.viewer".to_string(), 1259)]);

        let issues = match validate_update_deltas(Some(&json!({
            "deltas": [{"oldRole": "datalens.viewer", "newRole": "datalens.editor", "subjectId": "12$"}]
        }))) {
            Err(DeltasError::Validation(issues)) => issues,
            _ => panic!("expected validation error"),
        };
        assert_eq!(issues[0]["path"], json!(["deltas"]));
    }

    #[test]
    fn path_id_decoding_is_lenient_like_node() {
        assert_eq!(decode_path_id("123").ok(), Some(1259));
        assert!(decode_path_id("12$").is_err());
        match decode_path_id("3p837si3kczqp") {
            Ok(id) => assert_eq!(id, 1_844_674_407_370_955_161),
            Err(_) => panic!("canonical id must decode"),
        }
    }
}
