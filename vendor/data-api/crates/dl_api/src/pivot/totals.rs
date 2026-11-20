//! `dl_api_lib.request_model.normalization.drm_normalizer_pivot` (the totals part).
//!
//! Totals turn one pivot query into a *set* of queries. `PivotTotalsNormalizerHelper` walks
//! the requested totals and, for each of them, appends a block in which
//!
//! - the dimensions of the axis the total belongs to are replaced, from `level` on, by
//!   `template` placeholders (`FieldRole.template`, a constant value);
//! - the dimensions of the other axis stay in place;
//! - every measure gets the `total` role.
//!
//! Every block *clones* the legend items it selects: the merged stream is a union of blocks,
//! and a cell points at its own block's item (that is what makes a total row's dimension cell
//! distinguishable from a data cell of the same dimension). The new ids are appended to the
//! pivot items, so `liid -> piid` still resolves them to the same row/column/measure.
//!
//! The port keeps the shape and simplifies one detail: a `template` placeholder is the
//! constant `""`, and the compiler has no `SELECT '' AS ...` support, so the constant is
//! injected while the block's rows are decoded (see [`TotalCell::Template`]) instead of being
//! selected. The merged stream is the same either way.
//!
//! Pseudo-dimensions (`Measure Names`, `Dimension Names`) are never selected: they are legend
//! only in the base block too, and the transposer synthesizes their cells from the measures.

use std::collections::HashSet;

use dl_core::{Dataset, FieldRef, FieldType};
use dl_query::{compile, Dialect, OutputColumn, QuerySpec, Role, SelectItem};

use super::PivotError;
use crate::dto::{
    LegendItem, PivotRole, PivotSpec, PivotSpecItem, PivotTotals, PivotTotalsItem, RoleSpec,
    Visibility,
};
use crate::error::ApiError;
use crate::legend::{
    IdGenerator, Legend, LegendEntry, ITEM_TYPE_PLACEHOLDER, PLACEHOLDER_ID, PLACEHOLDER_TITLE,
};

/// `BaseBlockSpec.DEFAULT_BLOCK_ID`: the block the request itself is.
const BASE_BLOCK_ID: i64 = 0;

/// Where one cell of a totals row comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TotalCell {
    /// A column of the block's own `SELECT`: `index` into its decoded row.
    Column { index: usize },
    /// A `template` placeholder: the constant `""` the dropped dimension turned into.
    Template,
}

/// One extra query of a totals request (`PivotTotalsNormalizerHelper.gen_single_dir_totals`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TotalBlock {
    /// `RawBlockSpec.block_id`: the base block is `0`, the totals blocks follow it.
    pub block_id: i64,
    pub sql: String,
    /// The block's `SELECT` list, read by [`TotalCell::Column`].
    pub columns: Vec<OutputColumn>,
    /// The row layout of the merged stream: `(legend_item_id, source)` per cell, in order.
    pub cells: Vec<(i64, TotalCell)>,
}

impl TotalBlock {
    /// The ids of one block row, i.e. the `legend` half of a merged row.
    pub fn row_ids(&self) -> Vec<i64> {
        self.cells
            .iter()
            .map(|(legend_item_id, _)| *legend_item_id)
            .collect()
    }
}

/// What [`normalize`] builds: the extra blocks, the extended legend and the patched
/// `pivot.structure` (the normalizer reorders it: columns, rows, then measures).
#[derive(Debug, Clone, PartialEq)]
pub struct TotalsPlan {
    pub blocks: Vec<TotalBlock>,
    pub legend: Legend,
    pub structure: Vec<PivotSpecItem>,
}

impl TotalsPlan {
    /// The request `pivot` with the patched structure and no totals left
    /// (`_normalize_totals` drops them once they are expanded into blocks).
    pub fn patched_spec(&self, spec: &PivotSpec) -> PivotSpec {
        PivotSpec {
            structure: self.structure.clone(),
            with_totals: None,
            totals: None,
            ..spec.clone()
        }
    }
}

/// `PivotSpecNormalizer.normalize_spec` for the totals part: expand the totals of `spec` into
/// extra block plans, or `None` when the request asks for none (or cannot have any).
///
/// `base` is the base query spec: the blocks inherit its filters, parameters, limit and
/// `aggregate` flag, because Python rebuilds them from the same `RawQuerySpecUnion`.
/// `order_by` is *not* inherited: it is localized to the base block in Python too
/// (`order_by_specs` all get `block_id=base_block_id`), and a total block does not select the
/// dimensions the sort refers to.
pub fn normalize(
    dataset: &Dataset,
    spec: &PivotSpec,
    legend: &Legend,
    base: &QuerySpec,
    dialect: Dialect,
) -> Result<Option<TotalsPlan>, ApiError> {
    if !spec.wants_totals() {
        return Ok(None);
    }
    let mut builder = Builder::new(dataset, spec, legend, base, dialect);
    if builder.measures.is_empty() && builder.annotations.is_empty() {
        // `if not measures: return pivot_spec, raw_query_spec_union` — totals are impossible,
        // and the port leaves the structure untouched as well (`totals` stays in the spec, as
        // in Python, where nothing reads it any more).
        return Ok(None);
    }
    builder.push_blocks()?;
    Ok(Some(builder.finish()))
}

/// `PivotSpecNormalizer._normalize_simple_totals_from_flag`: `with_totals: true` means one
/// total per axis at level `0`; an explicit `totals` object wins over the flag.
fn expand_totals(spec: &PivotSpec) -> PivotTotals {
    match &spec.totals {
        Some(totals) => totals.clone(),
        None => {
            // `RawPivotTotalsItemSpec(level=0)`: total *every* dimension of the axis, which is
            // what `with_totals: true` means. The schema default (`level: None`) is a total
            // that keeps all of them, so it must not be used here.
            let total = PivotTotalsItem { level: Some(0) };
            PivotTotals {
                rows: vec![total.clone()],
                columns: vec![total],
            }
        }
    }
}

/// `PivotTotalsNormalizerHelper`, as a builder over one request.
struct Builder<'a> {
    dataset: &'a Dataset,
    /// The request legend, i.e. the base block's.
    legend: &'a Legend,
    base: &'a QuerySpec,
    dialect: Dialect,
    /// `raw_pivot_items`: the structure being patched (new ids are appended to its items).
    structure: Vec<PivotSpecItem>,
    /// Indices of the pivot items of each role, in structure order.
    rows: Vec<usize>,
    columns: Vec<usize>,
    measures: Vec<usize>,
    /// `measures = strict_measures + annotations`: annotations follow the measures in the
    /// structure, but a total never selects them (`Totals are not annotated`).
    annotations: Vec<usize>,
    totals: PivotTotals,
    /// The extended legend: the request legend plus one item per block cell.
    legend_out: Legend,
    /// Where a new item goes, i.e. right after the last selected one (`iter_item_specs` order).
    insert_at: usize,
    id_gen: IdGenerator,
    block_id_gen: IdGenerator,
    blocks: Vec<TotalBlock>,
}

impl<'a> Builder<'a> {
    fn new(
        dataset: &'a Dataset,
        spec: &PivotSpec,
        legend: &'a Legend,
        base: &'a QuerySpec,
        dialect: Dialect,
    ) -> Self {
        let of_role = |role: PivotRole| -> Vec<usize> {
            spec.structure
                .iter()
                .enumerate()
                .filter(|(_, item)| item.role_spec.role == role)
                .map(|(index, _)| index)
                .collect()
        };
        let insert_at = legend
            .entries
            .iter()
            .rposition(|entry| entry.select_index.is_some())
            .map_or(legend.entries.len(), |index| index + 1);
        Self {
            dataset,
            legend,
            base,
            dialect,
            structure: spec.structure.clone(),
            rows: of_role(PivotRole::Row),
            columns: of_role(PivotRole::Column),
            measures: of_role(PivotRole::Measure),
            annotations: of_role(PivotRole::Annotation),
            totals: expand_totals(spec),
            legend_out: legend.clone(),
            insert_at,
            id_gen: IdGenerator::new(legend.used_ids()),
            block_id_gen: IdGenerator::new([BASE_BLOCK_ID]),
            blocks: Vec::new(),
        }
    }

    /// Rows, then columns, then their intersections, exactly like `_normalize_totals`.
    fn push_blocks(&mut self) -> Result<(), ApiError> {
        // A `Measure Names`-only axis has no dimension to total.
        let mname_ids: HashSet<i64> = self
            .legend
            .measure_name_ids()
            .into_iter()
            .chain(self.legend.dimension_name_ids())
            .collect();
        let rows = self.rows.clone();
        let columns = self.columns.clone();
        if rows.is_empty() || !self.has_plain_dimension(&rows, &mname_ids) {
            self.totals.rows.clear();
        }
        if columns.is_empty() || !self.has_plain_dimension(&columns, &mname_ids) {
            self.totals.columns.clear();
        }

        let row_totals: Vec<Option<i64>> = self.totals.rows.iter().map(|item| item.level).collect();
        let column_totals: Vec<Option<i64>> =
            self.totals.columns.iter().map(|item| item.level).collect();

        for level in &row_totals {
            self.push_block(&rows, *level, &columns, None)?;
        }
        for level in &column_totals {
            self.push_block(&columns, *level, &rows, None)?;
        }
        for row_level in &row_totals {
            for column_level in &column_totals {
                self.push_block(&rows, *row_level, &columns, *column_level)?;
            }
        }
        Ok(())
    }

    /// `non_mname_row_dims`: at least one liid that is not a pseudo-dimension.
    fn has_plain_dimension(&self, indices: &[usize], mname_ids: &HashSet<i64>) -> bool {
        indices.iter().any(|index| {
            self.structure[*index]
                .legend_item_ids
                .iter()
                .any(|legend_item_id| !mname_ids.contains(legend_item_id))
        })
    }
    /// `gen_single_dir_totals`: one block for one total (and, for an intersection, for a pair).
    fn push_block(
        &mut self,
        this: &[usize],
        this_level: Option<i64>,
        other: &[usize],
        other_level: Option<i64>,
    ) -> Result<(), ApiError> {
        let mut select: Vec<SelectItem> = Vec::new();
        let mut cells: Vec<(i64, TotalCell)> = Vec::new();
        self.push_axis(this, this_level, &mut select, &mut cells)?;
        self.push_axis(other, other_level, &mut select, &mut cells)?;

        for index in self.measures.clone() {
            let base = self.main_item(index)?;
            let legend_item_id = self.push_item(index, total_item(&base));
            cells.push((
                legend_item_id,
                TotalCell::Column {
                    index: select.len(),
                },
            ));
            select.push(select_item(&base, Role::Total));
        }

        let block_id = self.block_id_gen.generate_id();
        let spec = QuerySpec {
            select,
            // The sorter orders the table; `order_by` stays with the base block in Python too.
            order_by: Vec::new(),
            ..self.base.clone()
        };
        let mut compiled = compile(self.dataset, &spec, self.dialect).map_err(ApiError::from)?;
        for (legend_item_id, cell) in &cells {
            if let TotalCell::Column { index } = cell {
                compiled.columns[*index].legend_item_id = *legend_item_id;
            }
        }
        self.blocks.push(TotalBlock {
            block_id,
            sql: compiled.sql,
            columns: compiled.columns,
            cells,
        });
        Ok(())
    }

    /// One axis of a block: dimensions below `level` stay, the rest become templates.
    fn push_axis(
        &mut self,
        indices: &[usize],
        level: Option<i64>,
        select: &mut Vec<SelectItem>,
        cells: &mut Vec<(i64, TotalCell)>,
    ) -> Result<(), ApiError> {
        for (position, index) in indices.iter().enumerate() {
            let base = self.main_item(*index)?;
            // A pseudo-dimension (`Measure Names`) is not a dimension of the block: Python's
            // `resolve_main_legend_item` skips it because its `block_id` is `None`, so it gets
            // neither a template placeholder nor a `SELECT` item — the transposer builds its
            // cells from the measures. Checking this before the level matters: a totals row
            // (`level = 0`) is "above" every position, and a placeholder appended to the
            // `Measure Names` item would mix two item types in one pivot item.
            if is_pseudo_dimension(&base) {
                continue;
            }
            if is_above_level(position, level) {
                cells.push((
                    self.push_item(*index, template_item(&base)),
                    TotalCell::Template,
                ));
                continue;
            }
            let legend_item_id = self.push_item(*index, dimension_item(&base));
            cells.push((
                legend_item_id,
                TotalCell::Column {
                    index: select.len(),
                },
            ));
            select.push(select_item(&base, Role::Row));
        }
        Ok(())
    }

    /// `resolve_main_legend_item`: the first liid of the item that belongs to the base block.
    ///
    /// The ids of the extra blocks are appended, so an item's base ids always come first.
    fn main_item(&self, index: usize) -> Result<LegendItem, ApiError> {
        let base_len = self.legend.entries.len();
        let resolved = self.structure[index]
            .legend_item_ids
            .iter()
            .find_map(|legend_item_id| {
                let position = self
                    .legend
                    .entries
                    .iter()
                    .position(|entry| entry.item.legend_item_id == *legend_item_id)?;
                (position < base_len).then(|| self.legend.entries[position].item.clone())
            });
        resolved.ok_or_else(|| {
            let legend_item_id = self.structure[index]
                .legend_item_ids
                .first()
                .copied()
                .unwrap_or_default();
            ApiError::from(PivotError::UnknownItem(legend_item_id))
        })
    }

    /// `make_legend_item` + `_rewrite_pivot_items`: a new legend item, and its id added to the
    /// pivot item it was cloned from.
    fn push_item(&mut self, index: usize, mut item: LegendItem) -> i64 {
        let legend_item_id = self.id_gen.generate_id();
        item.legend_item_id = legend_item_id;
        self.legend_out.entries.insert(
            self.insert_at,
            LegendEntry {
                item,
                in_result_set: true,
                select_index: None,
                block_id: None,
            },
        );
        self.insert_at += 1;
        self.structure[index].legend_item_ids.push(legend_item_id);
        legend_item_id
    }

    fn finish(self) -> TotalsPlan {
        // `_normalize_totals` reorders the structure: columns, rows, measures (annotations
        // included), everything else.
        let mut structure: Vec<PivotSpecItem> = Vec::with_capacity(self.structure.len());
        let groups: [&Vec<usize>; 4] =
            [&self.columns, &self.rows, &self.measures, &self.annotations];
        for indices in groups {
            structure.extend(indices.iter().map(|index| self.structure[*index].clone()));
        }
        let placed: HashSet<usize> = groups
            .iter()
            .flat_map(|indices| indices.iter())
            .copied()
            .collect();
        structure.extend(
            self.structure
                .iter()
                .enumerate()
                .filter(|(index, _)| !placed.contains(index))
                .map(|(_, item)| item.clone()),
        );
        TotalsPlan {
            blocks: self.blocks,
            legend: self.legend_out,
            structure,
        }
    }
}

/// `gen_single_dir_totals`: `this_level is None or item_idx < this_level` keeps a dimension.
fn is_above_level(position: usize, level: Option<i64>) -> bool {
    match level {
        None => false,
        Some(level) => position as i64 >= level,
    }
}

/// `spec_is_field_name_pseudo_dimension`: `Measure Names`/`Dimension Names` have no field.
fn is_pseudo_dimension(item: &LegendItem) -> bool {
    item.is_pseudo_dimension()
}

/// `T_DIMENSION`: the same field, `row` role, an own legend item.
fn dimension_item(base: &LegendItem) -> LegendItem {
    LegendItem {
        role_spec: RoleSpec::row(),
        ..base.clone()
    }
}

/// `T_TEMPLATE`: the placeholder a dropped dimension turns into, same data type.
fn template_item(base: &LegendItem) -> LegendItem {
    LegendItem {
        id: PLACEHOLDER_ID.to_string(),
        title: PLACEHOLDER_TITLE.to_string(),
        field_type: FieldType::Dimension,
        item_type: ITEM_TYPE_PLACEHOLDER.to_string(),
        role_spec: RoleSpec {
            role: Role::Template,
            template: Some(String::new()),
            visibility: Some(Visibility::Visible),
            ..RoleSpec::default()
        },
        ..base.clone()
    }
}

/// `T_MEASURE` with the `total` role: the measure aggregated over the dropped dimensions.
fn total_item(base: &LegendItem) -> LegendItem {
    LegendItem {
        role_spec: RoleSpec::new(Role::Total),
        ..base.clone()
    }
}

fn select_item(base: &LegendItem, role: Role) -> SelectItem {
    SelectItem::new(FieldRef::id(base.id.clone()), role)
}
