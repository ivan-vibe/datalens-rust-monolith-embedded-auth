use dl_core::{Aggregation, CalcMode, Dataset, Field, FieldRef, Source, UserDataType};
use dl_query::{compile, Dialect, QuerySpec, Role, SelectItem};

fn field(guid: &str, title: &str, dtype: UserDataType, aggregation: Aggregation) -> Field {
    Field {
        guid: guid.to_string(),
        title: title.to_string(),
        initial_data_type: Some(dtype),
        data_type: Some(dtype),
        aggregation,
        source: title.to_string(),
        ..Field::default()
    }
}

#[test]
fn probe_avg_fixed_over_measure() {
    let mut dataset = Dataset {
        sources: vec![Source::table("src", "public.sales")],
        ..Dataset::default()
    };
    dataset
        .fields
        .push(field("f_region", "Region", UserDataType::String, Aggregation::None));
    dataset
        .fields
        .push(field("f_num", "Num", UserDataType::Float, Aggregation::Sum));
    dataset
        .fields
        .push(field("f_den", "Den", UserDataType::Float, Aggregation::Sum));
    let mut pct = field("f_pct", "Pct", UserDataType::Float, Aggregation::None);
    pct.calc_mode = CalcMode::Formula;
    pct.formula = "[Num] / [Den]".to_string();
    dataset.fields.push(pct);
    let mut lod = field("f_lod", "Lod", UserDataType::Float, Aggregation::None);
    lod.calc_mode = CalcMode::Formula;
    lod.formula = "AVG([Pct] FIXED [Region])".to_string();
    dataset.fields.push(lod);

    let spec = QuerySpec {
        select: vec![
            SelectItem::new(FieldRef::id("f_region"), Role::Row),
            SelectItem::new(FieldRef::id("f_lod"), Role::Measure),
        ],
        ..QuerySpec::default()
    };
    let compiled = compile(&dataset, &spec, Dialect::Postgres).expect("compiles");
    println!("SQL:\n{}", compiled.sql);
}
