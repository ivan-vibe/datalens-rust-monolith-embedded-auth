//! `src/controllers/color-palettes/*` and `src/services/color-palettes/*`.

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::api::tenants::{
    invalid_type_issue, missing_field_issue, parse_query, read_json_body, received_type,
};
use crate::error::ApiError;
use crate::id::{decode_id, encode_id};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    crate::api::dual_router(vec![
        (
            "/v1/color-palettes",
            get(list_palettes).post(create_palette),
        ),
        (
            "/v1/color-palettes/{palette_id}",
            get(get_palette).delete(delete_palette),
        ),
        (
            "/v1/color-palettes/{palette_id}/update",
            post(update_palette),
        ),
    ])
}

const MAX_PALETTES_COUNT: i64 = 50;

const PALETTE_COLUMNS: &str = "color_palette_id, display_name, colors, is_default, is_gradient";

const COLOR_PATTERN: &str = "/^#([a-fA-F0-9]{6}|[a-fA-F0-9]{3})$/";

#[derive(Debug, Clone, sqlx::FromRow)]
struct PaletteRow {
    color_palette_id: i64,
    display_name: String,
    colors: Value,
    is_default: bool,
    is_gradient: bool,
}

/// `color-palette-model.ts` — exact field order of the zod object.
fn palette_model(palette: &PaletteRow) -> Value {
    json!({
        "colorPaletteId": encode_id(palette.color_palette_id),
        "displayName": palette.display_name,
        "colors": palette.colors,
        "isDefault": palette.is_default,
        "isGradient": palette.is_gradient,
    })
}

fn regex_color_issue(index: usize) -> Value {
    json!({
        "origin": "string",
        "code": "invalid_format",
        "format": "regex",
        "pattern": COLOR_PATTERN,
        "path": ["colors", index],
        "message": format!("Invalid string: must match pattern {COLOR_PATTERN}"),
    })
}

/// `^#([a-fA-F0-9]{6}|[a-fA-F0-9]{3})$` — three or six ASCII hex digits.
fn is_hex_color(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.first() != Some(&b'#') {
        return false;
    }

    let digits = &bytes[1..];
    matches!(digits.len(), 3 | 6) && digits.iter().all(u8::is_ascii_hexdigit)
}

fn body_issues(body: &Value) -> Vec<Value> {
    let map = match body {
        Value::Object(map) => map,
        other => return vec![invalid_type_issue("object", vec![], received_type(other))],
    };

    let mut issues = Vec::new();

    match map.get("displayName") {
        Some(Value::String(_)) => {}
        Some(other) => issues.push(invalid_type_issue(
            "string",
            vec![json!("displayName")],
            received_type(other),
        )),
        None => issues.push(missing_field_issue("string", "displayName")),
    }

    match map.get("colors") {
        Some(Value::Array(colors)) => {
            for (index, color) in colors.iter().enumerate() {
                match color {
                    Value::String(color) => {
                        if !is_hex_color(color) {
                            issues.push(regex_color_issue(index));
                        }
                    }
                    other => issues.push(invalid_type_issue(
                        "string",
                        vec![json!("colors"), json!(index)],
                        received_type(other),
                    )),
                }
            }
        }
        Some(other) => issues.push(invalid_type_issue(
            "array",
            vec![json!("colors")],
            received_type(other),
        )),
        None => issues.push(missing_field_issue("array", "colors")),
    }

    for field in ["isGradient", "isDefault"] {
        match map.get(field) {
            Some(Value::Bool(_)) => {}
            Some(other) => issues.push(invalid_type_issue(
                "boolean",
                vec![json!(field)],
                received_type(other),
            )),
            None => issues.push(missing_field_issue("boolean", field)),
        }
    }

    issues
}

async fn list_all(
    state: &AppState,
    color_palette_id: Option<i64>,
) -> Result<Vec<PaletteRow>, ApiError> {
    let sql = match color_palette_id {
        Some(_) => format!(
            "SELECT {PALETTE_COLUMNS} FROM color_palettes \
             WHERE tenant_id = $1 AND color_palette_id = $2 LIMIT 50"
        ),
        None => format!(
            "SELECT {PALETTE_COLUMNS} FROM color_palettes \
             WHERE tenant_id = $1 LIMIT 50"
        ),
    };

    let mut query = sqlx::query_as::<_, PaletteRow>(&sql).bind(&state.config.tenant_id_override);

    if let Some(color_palette_id) = color_palette_id {
        query = query.bind(color_palette_id);
    }

    Ok(query.fetch_all(&state.db.primary).await?)
}

/// Minimal `qs` model of `filters` for the list endpoint.
#[derive(Debug, PartialEq)]
enum Filters {
    Absent,
    /// A value that is not an object (`?filters=bad`) — zod rejects it.
    NotObject(&'static str),
    Object(Option<ColorPaletteIdFilter>),
}

#[derive(Debug, PartialEq)]
enum ColorPaletteIdFilter {
    String(String),
    /// `?filters[colorPaletteId]=a&filters[colorPaletteId]=b` — an array.
    Array,
    /// `?filters[colorPaletteId][x]=y` — an object.
    OtherObject,
}

fn parse_filters(mut query: crate::api::tenants::QueryMap) -> Filters {
    if !query
        .keys()
        .any(|key| key == "filters" || key.starts_with("filters["))
    {
        return Filters::Absent;
    }

    if let Some(values) = query.remove("filters") {
        return match values.len() {
            0 => Filters::NotObject("undefined"),
            1 => Filters::NotObject("string"),
            _ => Filters::NotObject("array"),
        };
    }

    // Repeatable parsing order of `qs` is not deterministic; only the shape
    // matters for validation.
    let mut filter = None;

    for key in query.keys() {
        if key.starts_with("filters[") {
            let inner = key.trim_start_matches("filters[").trim_end_matches(']');

            if inner == "colorPaletteId" {
                let count = query.get(key).map(Vec::len).unwrap_or(0);
                filter = if count > 1 {
                    Some(ColorPaletteIdFilter::Array)
                } else {
                    Some(ColorPaletteIdFilter::String(
                        query
                            .get(key)
                            .and_then(|values| values.first())
                            .cloned()
                            .unwrap_or_default(),
                    ))
                };
            } else if inner.starts_with("colorPaletteId[") {
                filter = Some(ColorPaletteIdFilter::OtherObject);
            }
        }
    }

    Filters::Object(filter)
}

fn filters_issues(filters: &Filters) -> Vec<Value> {
    match filters {
        Filters::Absent | Filters::Object(None) => Vec::new(),
        Filters::Object(Some(ColorPaletteIdFilter::String(_))) => Vec::new(),
        Filters::Object(Some(ColorPaletteIdFilter::Array)) => vec![invalid_type_issue(
            "string",
            vec![json!("filters"), json!("colorPaletteId")],
            "array",
        )],
        Filters::Object(Some(ColorPaletteIdFilter::OtherObject)) => vec![invalid_type_issue(
            "string",
            vec![json!("filters"), json!("colorPaletteId")],
            "object",
        )],
        Filters::NotObject(received) => {
            vec![invalid_type_issue(
                "object",
                vec![json!("filters")],
                received,
            )]
        }
    }
}

/// `GET /v1/color-palettes`.
async fn list_palettes(
    State(state): State<AppState>,
    RawQuery(raw_query): RawQuery,
) -> Result<Response, ApiError> {
    let filters = parse_filters(parse_query(raw_query.as_deref().unwrap_or_default()));

    let issues = filters_issues(&filters);
    if !issues.is_empty() {
        return Err(ApiError::validation_issues(issues));
    }

    // `getColorPalettesList`: an undecodable filter is not an error, it simply
    // matches nothing.
    let color_palette_id = match filters {
        Filters::Object(Some(ColorPaletteIdFilter::String(id))) if !id.is_empty() => {
            match decode_id(&id) {
                Ok(id) => Some(id),
                Err(_) => return Ok(Json(json!([])).into_response()),
            }
        }
        _ => None,
    };

    let palettes = list_all(&state, color_palette_id).await?;
    let models: Vec<Value> = palettes.iter().map(palette_model).collect();

    Ok(Json(Value::Array(models)).into_response())
}

/// `GET /v1/color-palettes/:colorPaletteId`.
async fn get_palette(
    State(state): State<AppState>,
    Path(raw_palette_id): Path<String>,
) -> Result<Response, ApiError> {
    let color_palette_id = match decode_id(&raw_palette_id) {
        Ok(id) => id,
        Err(_) => {
            return Err(ApiError::validation(json!([ApiError::encoded_id_issue(
                "colorPaletteId",
                &raw_palette_id
            )])))
        }
    };

    let sql = format!("SELECT {PALETTE_COLUMNS} FROM color_palettes WHERE color_palette_id = $1");

    let palettes = sqlx::query_as::<_, PaletteRow>(&sql)
        .bind(color_palette_id)
        .fetch_all(&state.db.replica)
        .await?;

    let models: Vec<Value> = palettes.iter().map(palette_model).collect();

    Ok(Json(Value::Array(models)).into_response())
}

/// `checkColorPalettesAdmin` of the open-source registry is a no-op.
fn too_many_palettes(is_gradient: bool) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        Some("TOO_MANY_COLOR_PALETTES"),
        format!(
            "Too many color palettes: only {MAX_PALETTES_COUNT}, {} color palettes are allowed",
            if is_gradient { "gradient" } else { "plain" }
        ),
    )
    .with_details(json!({ "maxPalettesCount": MAX_PALETTES_COUNT }))
}

/// `POST /v1/color-palettes`.
async fn create_palette(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let body = match read_json_body(&headers, body).await {
        Ok(body) => body,
        Err(response) => return Ok(response),
    };

    let issues = body_issues(&body);
    if !issues.is_empty() {
        return Err(ApiError::validation_issues(issues));
    }

    let display_name = body["displayName"].as_str().expect("validated above");
    let colors = body["colors"].clone();
    let is_gradient = body["isGradient"].as_bool().expect("validated above");
    let is_default = body["isDefault"].as_bool().expect("validated above");

    // `getColorPalettesCount` reads the replica; the limit check is `> 50`.
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM color_palettes WHERE tenant_id = $1 AND is_gradient = $2",
    )
    .bind(&state.config.tenant_id_override)
    .bind(is_gradient)
    .fetch_one(&state.db.replica)
    .await?;

    if count > MAX_PALETTES_COUNT {
        return Err(too_many_palettes(is_gradient));
    }

    let colors_json = serde_json::to_string(&colors).map_err(|error| {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, None, error.to_string())
    })?;

    let mut tx = state.db.primary.begin().await?;

    if is_default {
        sqlx::query(
            "UPDATE color_palettes SET is_default = FALSE \
             WHERE is_default = TRUE AND is_gradient = $1 AND tenant_id = $2",
        )
        .bind(is_gradient)
        .bind(&state.config.tenant_id_override)
        .execute(&mut *tx)
        .await?;
    }

    let sql = format!(
        "INSERT INTO color_palettes \
         (display_name, name, colors, tenant_id, is_default, is_gradient) \
         VALUES ($1, $2, $3::jsonb, $4, $5, $6) \
         RETURNING {PALETTE_COLUMNS}"
    );

    let palette = sqlx::query_as::<_, PaletteRow>(&sql)
        .bind(display_name)
        .bind(display_name.to_lowercase())
        .bind(colors_json)
        .bind(&state.config.tenant_id_override)
        .bind(is_default)
        .bind(is_gradient)
        .fetch_one(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(Json(palette_model(&palette)).into_response())
}

/// `POST /v1/color-palettes/:colorPaletteId/update`.
async fn update_palette(
    State(state): State<AppState>,
    Path(raw_palette_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    // Order of `withContract`/`makeReqParser`: the body is parsed by
    // body-parser first (content-type and JSON errors win), then params are
    // validated before the body schema.
    let body = match read_json_body(&headers, body).await {
        Ok(body) => body,
        Err(response) => return Ok(response),
    };

    let color_palette_id = match decode_id(&raw_palette_id) {
        Ok(id) => id,
        Err(_) => {
            return Err(ApiError::validation(json!([ApiError::encoded_id_issue(
                "colorPaletteId",
                &raw_palette_id
            )])))
        }
    };

    let issues = body_issues(&body);
    if !issues.is_empty() {
        return Err(ApiError::validation_issues(issues));
    }

    let display_name = body["displayName"].as_str().expect("validated above");
    let colors = body["colors"].clone();
    let is_gradient = body["isGradient"].as_bool().expect("validated above");
    let is_default = body["isDefault"].as_bool().expect("validated above");

    let colors_json = serde_json::to_string(&colors).map_err(|error| {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, None, error.to_string())
    })?;

    let mut tx = state.db.primary.begin().await?;

    if is_default {
        sqlx::query(
            "UPDATE color_palettes SET is_default = FALSE \
             WHERE is_default = TRUE AND is_gradient = $1 AND tenant_id = $2",
        )
        .bind(is_gradient)
        .bind(&state.config.tenant_id_override)
        .execute(&mut *tx)
        .await?;
    }

    let sql = format!(
        "UPDATE color_palettes \
         SET display_name = $3, name = $4, is_default = $5, is_gradient = $6, colors = $7::jsonb \
         WHERE color_palette_id = $1 AND tenant_id = $2 \
         RETURNING {PALETTE_COLUMNS}"
    );

    let palette = sqlx::query_as::<_, PaletteRow>(&sql)
        .bind(color_palette_id)
        .bind(&state.config.tenant_id_override)
        .bind(display_name)
        .bind(display_name.to_lowercase())
        .bind(is_default)
        .bind(is_gradient)
        .bind(colors_json)
        .fetch_optional(&mut *tx)
        .await?;

    let palette = match palette {
        Some(palette) => palette,
        None => {
            tx.rollback().await?;
            return Err(palette_not_exists());
        }
    };

    tx.commit().await?;

    Ok(Json(palette_model(&palette)).into_response())
}

fn palette_not_exists() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        Some("COLOR_PALETTE_NOT_EXISTS"),
        "The pallete doesn't exist",
    )
}

/// `DELETE /v1/color-palettes/:colorPaletteId`.
async fn delete_palette(
    State(state): State<AppState>,
    Path(raw_palette_id): Path<String>,
) -> Result<Response, ApiError> {
    let color_palette_id = match decode_id(&raw_palette_id) {
        Ok(id) => id,
        Err(_) => {
            return Err(ApiError::validation(json!([ApiError::encoded_id_issue(
                "colorPaletteId",
                &raw_palette_id
            )])))
        }
    };

    let sql = format!(
        "DELETE FROM color_palettes \
         WHERE color_palette_id = $1 AND tenant_id = $2 \
         RETURNING {PALETTE_COLUMNS}"
    );

    let deleted = sqlx::query_as::<_, PaletteRow>(&sql)
        .bind(color_palette_id)
        .bind(&state.config.tenant_id_override)
        .fetch_optional(&state.db.primary)
        .await?;

    if deleted.is_none() {
        return Err(palette_not_exists());
    }

    Ok(StatusCode::OK.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_three_and_six_digit_hex_colors() {
        assert!(is_hex_color("#fff"));
        assert!(is_hex_color("#FFFFFF"));
        assert!(is_hex_color("#AbC123"));
        assert!(!is_hex_color("fff"));
        assert!(!is_hex_color("#ff"));
        assert!(!is_hex_color("#ffff"));
        assert!(!is_hex_color("#ggg"));
        assert!(!is_hex_color("#FFFFFG"));
        assert!(!is_hex_color(""));
    }

    #[test]
    fn body_issues_match_zod_order() {
        let issues = body_issues(&json!({}));
        assert_eq!(issues.len(), 4);
        assert_eq!(issues[0]["path"], json!(["displayName"]));
        assert_eq!(issues[1]["path"], json!(["colors"]));
        assert_eq!(issues[2]["path"], json!(["isGradient"]));
        assert_eq!(issues[3]["path"], json!(["isDefault"]));

        let issues = body_issues(&json!({
            "displayName": "x",
            "colors": ["#fff", "bad", 5],
            "isGradient": false,
            "isDefault": false,
        }));
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0]["code"], "invalid_format");
        assert_eq!(issues[0]["path"], json!(["colors", 1]));
        assert_eq!(issues[1]["code"], "invalid_type");
        assert_eq!(issues[1]["path"], json!(["colors", 2]));
    }

    #[test]
    fn filters_parse_like_qs_shapes() {
        assert_eq!(parse_filters(parse_query("")), Filters::Absent);
        assert_eq!(
            parse_filters(parse_query("filters=bad")),
            Filters::NotObject("string")
        );
        assert_eq!(
            parse_filters(parse_query("filters[colorPaletteId]=abc")),
            Filters::Object(Some(ColorPaletteIdFilter::String("abc".to_owned())))
        );
        assert_eq!(
            parse_filters(parse_query(
                "filters[colorPaletteId]=a&filters[colorPaletteId]=b"
            )),
            Filters::Object(Some(ColorPaletteIdFilter::Array))
        );
    }
}
