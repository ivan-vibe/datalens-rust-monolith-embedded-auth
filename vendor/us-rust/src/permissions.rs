//! `accessService` permissions of US 1.196.0.
//!
//! Ports the parts of the original registry plugin that matter for this
//! deployment (`accessServiceEnabled = AUTH_ENABLED`, `dlsEnabled = false`):
//!
//! * [`is_editor`] — `Workbook.isEditorOrAdmin()` (`datalens.editor|admin`);
//! * [`workbook_permissions`] / [`entry_permissions`] — `getAllPermissions` and
//!   `getEntryPermissionsByWorkbook` (`services/new/workbook/utils`);
//! * [`collection_permissions`] / [`root_collection_permissions`] —
//!   `Collection.getAllPermissions` and `getRootCollectionPermissions`;
//! * [`entry_access`] — `checkEntriesByPermission` for workbook / collection /
//!   folder entries (`services/new/entry/utils/check-entries-by-permission.js`);
//! * [`filter_entries_by_permission`] / [`locked_stub`] — `Navigation.getEntries`
//!   (`db/models/navigation/index.js`) plus `filterEntriesByIsLocked`;
//!   [`filter_collections_by_permission`] is the same gate for collection lists
//!   (`CollectionPermission.Browse` of `getCollectionsListByIds`);
//! * [`check_workbook_isolation`] — `check-workbook-isolation.js`.

use std::collections::HashSet;

use axum::http::HeaderMap;
use serde_json::{json, Map, Value};
use sqlx::PgPool;

use crate::auth::AuthUser;
use crate::error::ApiError;
use crate::id::decode_id;
use crate::state::AppState;

/// `UserRole.Editor` / `UserRole.Admin` (`components/auth/constants/role.ts`).
const ROLE_EDITOR: &str = "datalens.editor";
const ROLE_ADMIN: &str = "datalens.admin";

/// `DL_WORKBOOK_ID_HEADER` (`components/middlewares/resolve-isolation-ids.js`).
const WORKBOOK_ID_HEADER: &str = "x-dl-workbookid";

/// `isEditorOrAdmin()`: with `appAuthPolicy = disabled` (auth off) everybody is
/// an editor; otherwise the JWT roles decide.
pub fn is_editor(auth_enabled: bool, user: Option<&AuthUser>) -> bool {
    !auth_enabled
        || user.is_some_and(|user| {
            user.roles
                .iter()
                .any(|role| role == ROLE_EDITOR || role == ROLE_ADMIN)
        })
}

pub fn is_editor_for(state: &AppState, user: Option<&AuthUser>) -> bool {
    is_editor(state.config.auth_enabled, user)
}

/// `checkWorkbookPermission(Update|Delete|...)` / `Collection.checkPermission`
/// for this deployment: every mutation needs an editor/admin role.
pub fn require_editor(state: &AppState, user: Option<&AuthUser>) -> Result<(), ApiError> {
    if is_editor_for(state, user) {
        Ok(())
    } else {
        Err(ApiError::access_service_permission_denied())
    }
}

/// `checkCtxTenantId`: routes registered with `requireCtxTenantId` (all of them
/// are `/private/*`) need a resolved `ctx.info.tenantId`.
pub fn require_ctx_tenant_id(state: &AppState, is_private: bool) -> Result<(), ApiError> {
    require_ctx_tenant_id_value(&state.config.tenant_id_override, is_private)
}

fn require_ctx_tenant_id_value(tenant_id: &str, is_private: bool) -> Result<(), ApiError> {
    if is_private && tenant_id.is_empty() {
        return Err(ApiError::tenant_id_missing_in_context());
    }
    Ok(())
}

/// `Workbook.enableAllPermissions()` — the `includePermissionsInfo` response of
/// a workbook without the access service, or for an editor/admin.
pub fn full_workbook_permissions() -> Value {
    json!({
        "listAccessBindings": true,
        "updateAccessBindings": true,
        "limitedView": true,
        "view": true,
        "update": true,
        "copy": true,
        "move": true,
        "publish": true,
        "embed": true,
        "delete": true,
        "securityApprove": true,
    })
}

/// `Workbook.getAllPermissions()` — the view flags are always true, the write
/// flags follow `isEditorOrAdmin`.
pub fn workbook_permissions(editor: bool) -> Value {
    json!({
        "listAccessBindings": true,
        "updateAccessBindings": editor,
        "limitedView": true,
        "view": true,
        "update": editor,
        "copy": editor,
        "move": editor,
        "publish": editor,
        "embed": editor,
        "delete": editor,
        "securityApprove": editor,
    })
}

/// `Collection.getAllPermissions()`: `view`/`limitedView`/`browse` are always
/// true, every write flag follows `isEditorOrAdmin`.
pub fn collection_permissions(editor: bool) -> Value {
    json!({
        "listAccessBindings": true,
        "updateAccessBindings": editor,
        "createCollection": editor,
        "createWorkbook": editor,
        "createSharedEntry": editor,
        "createSparkCluster": editor,
        "createTrinoCluster": editor,
        "limitedView": true,
        "view": true,
        "browse": true,
        "update": editor,
        "copy": editor,
        "move": editor,
        "delete": editor,
        "securityManage": editor,
        "securityApprove": editor,
    })
}

/// `Collection.enableAllPermissions()` (`!accessServiceEnabled`, private routes).
pub fn full_collection_permissions() -> Value {
    collection_permissions(true)
}

/// `getRootCollectionPermissions`: `createCollectionInRoot` and
/// `createWorkbookInRoot` are `OrganizationPermission`s granted to editors.
pub fn root_collection_permissions(editor: bool) -> Value {
    json!({
        "createCollectionInRoot": editor,
        "createWorkbookInRoot": editor,
    })
}

/// `CollectionPermission.Browse` for this deployment. Permission maps are
/// uniform per role here, so list handlers can gate whole result sets with it.
pub fn collection_browsable(editor: bool) -> bool {
    collection_permissions(editor)
        .get("browse")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// `filterEntriesByPermission` analog for collection lists
/// (`getCollectionsListByIds`, `processPermissions`): a collection is returned
/// only when its permission set grants `browse`. `permissions` is the second
/// tuple item because access bindings can differ per collection.
pub fn filter_collections_by_permission(collections: Vec<(Value, Value)>) -> Vec<Value> {
    collections
        .into_iter()
        .filter(|(_, permissions)| {
            permissions
                .get("browse")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .map(|(collection, _)| collection)
        .collect()
}

/// `getEntryPermissionsByWorkbook`: `read` maps to `limitedView|view` (both true
/// here), `edit` to `update`, `admin` to `updateAccessBindings`.
pub fn entry_permissions(editor: bool) -> Value {
    json!({
        "execute": true,
        "read": true,
        "edit": editor,
        "admin": editor,
    })
}

/// `makeEntryWithFullPermissions` (folder entries and `enableAllPermissions`).
pub fn full_entry_permissions() -> Value {
    json!({
        "execute": true,
        "read": true,
        "edit": true,
        "admin": true,
    })
}

/// `makeEntryWithNoPermissions`.
pub fn no_entry_permissions() -> Value {
    json!({
        "execute": false,
        "read": false,
        "edit": false,
        "admin": false,
    })
}

/// Permission outcome of one navigation/relations entry.
#[derive(Debug, Clone)]
pub struct EntryAccess {
    pub is_locked: bool,
    /// `None` when the request did not ask for `includePermissionsInfo`.
    pub permissions: Option<Value>,
}

/// `checkEntriesByPermission` for a single entry.
///
/// * workbook entry: the parent workbook must exist, otherwise the entry is
///   locked with no permissions (`makeEntryWithNoPermissions`);
/// * collection entry: this build has no collection entries (the constructor
///   throws), so an unexpected row is locked rather than exposed;
/// * folder entry: `checkFolderEntriesByPermission` grants everything when DLS
///   is disabled.
pub fn entry_access(
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
    workbook_exists: bool,
    editor: bool,
    include_permissions_info: bool,
) -> EntryAccess {
    let (is_locked, permissions) = if workbook_id.is_some() {
        if workbook_exists {
            (false, entry_permissions(editor))
        } else {
            (true, no_entry_permissions())
        }
    } else if collection_id.is_some() {
        (true, no_entry_permissions())
    } else {
        (false, full_entry_permissions())
    };

    EntryAccess {
        is_locked,
        permissions: include_permissions_info.then_some(permissions),
    }
}

/// Rows returned by `DLS.checkBulkPermission(..., {includePermissionsInfo: true})`.
///
/// The US 1.196.0 open-source image ships the DLS registry class as a stub
/// (`registry/plugins/common/components/dls/dls.js`): `checkPermission` returns
/// `{}` and `checkBulkPermission` returns `[]`. A real DLS backend is a separate
/// registry plugin that is not part of this build, so the reference performs the
/// check and it always passes.
///
/// shortcut: DLS permissions are not evaluated; upgrade when an external DLS
/// service has to be honoured.
pub fn dls_bulk_permissions() -> Vec<Value> {
    Vec::new()
}

/// `checkExistenceEntriesWithInsufficientPermissions`: does any permission row
/// lack `permission`? An empty list (the shipped stub) means no.
pub fn dls_entries_with_insufficient_permissions(permissions: &[Value], permission: &str) -> bool {
    // `entriesWithPermissionsInfo.map(({permissions = {}}) => permissions)`.
    let empty = Value::Object(Map::new());
    permissions.iter().any(|row| {
        let permissions = row.get("permissions").unwrap_or(&empty);
        !permissions
            .get(permission)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    })
}

/// The DLS gate of `delete-entry.js` for the children of a folder:
/// `DLS.checkPermission(SetPermissions)` on the folder and
/// `checkExistenceEntriesWithInsufficientPermissions(Admin)` on the children.
/// `dls_enabled === false` skips both, exactly like the original.
pub fn dls_folder_delete_denied(dls_enabled: bool, permissions: &[Value]) -> bool {
    dls_enabled && dls_entries_with_insufficient_permissions(permissions, "admin")
}

/// `getWorkbooksListByIds` visibility test: live workbooks of the context tenant.
pub async fn existing_workbook_ids(
    pool: &PgPool,
    tenant_id: &str,
    workbook_ids: &[i64],
) -> Result<HashSet<i64>, ApiError> {
    if workbook_ids.is_empty() {
        return Ok(HashSet::new());
    }

    let rows: Vec<(i64,)> = sqlx::query_as(
        "SELECT workbook_id FROM workbooks \
         WHERE deleted_at IS NULL AND tenant_id = $1 AND workbook_id = ANY($2)",
    )
    .bind(tenant_id)
    .bind(workbook_ids.to_vec())
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|(workbook_id,)| workbook_id).collect())
}

pub fn is_locked(entry: &Value) -> bool {
    entry
        .get("isLocked")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn has_parent(entry: &Value) -> bool {
    ["workbookId", "collectionId"]
        .iter()
        .any(|field| entry.get(*field).is_some_and(|value| !value.is_null()))
}

/// `filterEntriesByPermission`: a locked entry that lives in a workbook or a
/// collection is removed from list responses.
pub fn filter_entries_by_permission(entries: Vec<Value>) -> Vec<Value> {
    entries
        .into_iter()
        .filter(|entry| !(has_parent(entry) && is_locked(entry)))
        .collect()
}

/// `filterEntriesByIsLocked`.
pub fn filter_entries_by_is_locked(entries: Vec<Value>) -> Vec<Value> {
    entries
        .into_iter()
        .filter(|entry| !is_locked(entry))
        .collect()
}

/// The `{isLocked, entryId, scope, type}` shape `Navigation.getEntries` returns
/// for a locked entry when `excludeLocked` is false.
pub fn locked_stub(entry: &Value) -> Value {
    let mut stub = Map::new();
    stub.insert("isLocked".to_owned(), Value::Bool(true));
    for field in ["entryId", "scope", "type"] {
        stub.insert(
            field.to_owned(),
            entry.get(field).cloned().unwrap_or(Value::Null),
        );
    }
    Value::Object(stub)
}

/// `resolveIsolationIds`: the decoded `x-dl-workbookid` header, if any. The
/// header format was already validated by `api::compat::guards`.
pub fn request_workbook_id(headers: &HeaderMap) -> Option<i64> {
    headers
        .get(WORKBOOK_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| decode_id(value).ok())
}

/// `checkWorkbookIsolation`: collection entries are checked by the collection
/// branch in the original and are skipped here; every other entry must belong to
/// the workbook named by the header.
pub fn check_workbook_isolation(
    headers: &HeaderMap,
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
) -> Result<(), ApiError> {
    check_workbook_isolation_id(request_workbook_id(headers), workbook_id, collection_id)
}

/// The header-free half of [`check_workbook_isolation`], for handlers that
/// already decoded the header.
pub fn check_workbook_isolation_id(
    request_workbook_id: Option<i64>,
    workbook_id: Option<i64>,
    collection_id: Option<i64>,
) -> Result<(), ApiError> {
    let Some(request_workbook_id) = request_workbook_id else {
        return Ok(());
    };

    if collection_id.is_some() {
        return Ok(());
    }

    if workbook_id != Some(request_workbook_id) {
        return Err(ApiError::workbook_isolation_interruption());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthUser;
    use crate::state::UserInfo;

    fn user_with_roles(roles: &[&str]) -> AuthUser {
        AuthUser {
            user: UserInfo::default(),
            roles: roles.iter().map(|role| (*role).to_owned()).collect(),
            token_type: None,
            access_token: String::new(),
        }
    }

    #[test]
    fn role_check_follows_editor_and_admin() {
        assert!(is_editor(true, Some(&user_with_roles(&["datalens.admin"]))));
        assert!(is_editor(
            true,
            Some(&user_with_roles(&["datalens.editor"]))
        ));
        assert!(!is_editor(
            true,
            Some(&user_with_roles(&["datalens.viewer"]))
        ));
        assert!(!is_editor(true, None));
        // AuthPolicy.disabled: every caller is an editor.
        assert!(is_editor(false, None));
    }

    #[test]
    fn viewer_entry_permissions_are_read_only() {
        let viewer = entry_permissions(false);
        assert_eq!(viewer["read"], json!(true));
        assert_eq!(viewer["execute"], json!(true));
        assert_eq!(viewer["edit"], json!(false));
        assert_eq!(viewer["admin"], json!(false));

        let editor = entry_permissions(true);
        assert_eq!(editor["edit"], json!(true));
        assert_eq!(editor["admin"], json!(true));

        let workbook = workbook_permissions(false);
        assert_eq!(workbook["view"], json!(true));
        assert_eq!(workbook["limitedView"], json!(true));
        assert_eq!(workbook["update"], json!(false));
        assert_eq!(workbook["delete"], json!(false));
    }

    #[test]
    fn missing_parent_workbook_locks_the_entry() {
        let access = entry_access(Some(1), None, false, true, true);
        assert!(access.is_locked);
        assert_eq!(access.permissions.unwrap()["read"], json!(false));

        let access = entry_access(Some(1), None, true, false, false);
        assert!(!access.is_locked);
        assert!(access.permissions.is_none());

        // Folder entries never lock.
        let access = entry_access(None, None, false, false, true);
        assert!(!access.is_locked);
        assert_eq!(access.permissions.unwrap()["edit"], json!(true));
    }

    #[test]
    fn locked_workbook_entries_are_filtered_and_stubbed() {
        let locked = json!({
            "entryId": "abc", "scope": "dataset", "type": "datalens.dataset",
            "workbookId": "wb", "isLocked": true,
        });
        let visible = json!({
            "entryId": "def", "scope": "dataset", "type": "datalens.dataset",
            "workbookId": "wb", "isLocked": false,
        });
        let folder = json!({
            "entryId": "ghi", "scope": "folder", "type": "datalens.folder",
            "workbookId": Value::Null, "isLocked": false,
        });

        let filtered =
            filter_entries_by_permission(vec![locked.clone(), visible.clone(), folder.clone()]);
        assert_eq!(filtered, vec![visible.clone(), folder]);

        let stubbed: Vec<Value> = vec![locked.clone()]
            .into_iter()
            .map(|entry| {
                if is_locked(&entry) {
                    locked_stub(&entry)
                } else {
                    entry
                }
            })
            .collect();
        assert_eq!(
            stubbed,
            vec![json!({
                "isLocked": true, "entryId": "abc", "scope": "dataset",
                "type": "datalens.dataset",
            })]
        );
        assert!(filter_entries_by_is_locked(vec![locked]).is_empty());
    }

    #[test]
    fn viewer_collection_permissions_are_visible_but_read_only() {
        let viewer = collection_permissions(false);
        for flag in ["view", "limitedView", "browse", "listAccessBindings"] {
            assert_eq!(viewer[flag], json!(true), "{flag}");
        }
        for flag in [
            "update",
            "delete",
            "move",
            "copy",
            "createCollection",
            "createWorkbook",
            "updateAccessBindings",
        ] {
            assert_eq!(viewer[flag], json!(false), "{flag}");
        }

        let editor = collection_permissions(true);
        for flag in ["update", "delete", "move", "createCollection"] {
            assert_eq!(editor[flag], json!(true), "{flag}");
        }
        assert_eq!(full_collection_permissions(), editor);
    }

    #[test]
    fn root_permissions_follow_the_editor_role() {
        assert_eq!(
            root_collection_permissions(false),
            json!({"createCollectionInRoot": false, "createWorkbookInRoot": false})
        );
        assert_eq!(
            root_collection_permissions(true),
            json!({"createCollectionInRoot": true, "createWorkbookInRoot": true})
        );
    }

    #[test]
    fn collection_list_filter_drops_entries_without_browse() {
        let browsable = json!({"collectionId": "a", "permissions": {"browse": true}});
        let hidden = json!({"collectionId": "b", "permissions": {"browse": false}});

        let kept = filter_collections_by_permission(vec![
            (browsable.clone(), json!({"browse": true, "view": true})),
            (hidden, json!({"browse": false})),
        ]);

        assert_eq!(kept, vec![browsable]);
    }

    #[test]
    fn missing_tenant_is_only_reported_on_private_routes() {
        assert_eq!(
            require_ctx_tenant_id_value("", true)
                .expect_err("private route without tenant")
                .code,
            Some("TENANT_ID_MISSING_IN_CONTEXT")
        );
        assert!(require_ctx_tenant_id_value("", false).is_ok());
        assert!(require_ctx_tenant_id_value("common", true).is_ok());
    }

    #[test]
    fn dls_stub_allows_folder_delete_in_both_modes() {
        // DLS disabled: the check is skipped entirely.
        assert!(!dls_folder_delete_denied(false, &[]));
        assert!(!dls_folder_delete_denied(
            false,
            &[json!({"permissions": {"admin": false}})]
        ));

        // DLS enabled with the stub shipped by the image: no permission rows, so
        // the folder delete proceeds (the pre-fix port answered 403 here).
        assert!(dls_bulk_permissions().is_empty());
        assert!(!dls_folder_delete_denied(true, &dls_bulk_permissions()));

        // A real DLS backend that reports a child without Admin still aborts it.
        assert!(dls_folder_delete_denied(
            true,
            &[json!({"permissions": {"admin": false}})]
        ));
        assert!(!dls_folder_delete_denied(
            true,
            &[json!({"permissions": {"admin": true}})]
        ));
    }

    #[test]
    fn isolation_header_must_match_the_entry_workbook() {
        use axum::http::HeaderValue;

        let mut headers = HeaderMap::new();
        headers.insert(
            WORKBOOK_ID_HEADER,
            HeaderValue::from_static("oy31411o0n5a7"),
        );
        let request = request_workbook_id(&headers).expect("header decodes");
        let other = request + 1;

        assert!(check_workbook_isolation(&headers, Some(request), None).is_ok());
        assert_eq!(
            check_workbook_isolation(&headers, Some(other), None)
                .expect_err("foreign workbook")
                .code,
            Some("WORKBOOK_ISOLATION_INTERRUPTION")
        );
        // A collection entry is checked by the collection branch in the original.
        assert!(check_workbook_isolation(&headers, None, Some(1)).is_ok());
        // Folder entries are not visible through a workbook-scoped request.
        assert!(check_workbook_isolation(&headers, None, None).is_err());

        assert!(check_workbook_isolation(&HeaderMap::new(), None, None).is_ok());
    }
}
