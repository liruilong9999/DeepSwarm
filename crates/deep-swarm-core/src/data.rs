use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufRead, BufReader},
    path::{Component, Path, PathBuf},
};

use serde_json::{Map, Value};

use crate::{
    CoreError,
    dsl::{DataFormat, DataSource},
};

const MAX_RECORDS: usize = 10_000;

#[derive(Clone, Debug)]
pub struct FixtureRoot(PathBuf);

impl FixtureRoot {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, CoreError> {
        let path = path
            .into()
            .canonicalize()
            .map_err(|error| CoreError::invalid(format!("fixture 根目录不可访问: {error}")))?;
        Ok(Self(path))
    }

    fn resolve(&self, relative: &str) -> Result<PathBuf, CoreError> {
        let path = Path::new(relative);
        if path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(CoreError::invalid(format!("数据源路径越界: {relative}")));
        }
        let resolved =
            self.0.join(path).canonicalize().map_err(|error| {
                CoreError::invalid(format!("数据源 {relative} 不可访问: {error}"))
            })?;
        if !resolved.starts_with(&self.0) {
            return Err(CoreError::invalid(format!(
                "数据源符号链接逃逸: {relative}"
            )));
        }
        Ok(resolved)
    }
}

#[derive(Clone, Debug)]
pub struct DataSet {
    pub source: DataSource,
    pub records: usize,
}

impl DataSet {
    pub fn read(&self, root: &FixtureRoot) -> Result<Vec<Value>, CoreError> {
        scan_source(root, &self.source, true).map(|(_, values)| values)
    }
}

pub(crate) fn preflight_sources(
    root: &FixtureRoot,
    sources: &[DataSource],
) -> Result<BTreeMap<String, DataSet>, CoreError> {
    let mut result = BTreeMap::new();
    for source in sources {
        if result.contains_key(&source.id) {
            return Err(CoreError::invalid(format!("重复数据源: {}", source.id)));
        }
        let records = scan_source(root, source, false)?.0;
        result.insert(
            source.id.clone(),
            DataSet {
                source: source.clone(),
                records,
            },
        );
    }
    Ok(result)
}

fn scan_source(
    root: &FixtureRoot,
    source: &DataSource,
    collect: bool,
) -> Result<(usize, Vec<Value>), CoreError> {
    let path = root.resolve(&source.path)?;
    match source.format {
        DataFormat::Csv => read_csv(&path, &source.path, collect),
        DataFormat::Jsonl => read_jsonl(&path, &source.path, collect),
    }
}

fn read_csv(path: &Path, display: &str, collect: bool) -> Result<(usize, Vec<Value>), CoreError> {
    let mut reader = csv::ReaderBuilder::new()
        .flexible(false)
        .from_path(path)
        .map_err(|error| CoreError::invalid(format!("CSV {display}: {error}")))?;
    let headers = reader
        .headers()
        .map_err(|error| CoreError::invalid(format!("CSV {display}: {error}")))?
        .clone();
    if headers.is_empty() {
        return Err(CoreError::invalid(format!("CSV {display} 缺少表头")));
    }
    let mut unique = BTreeSet::new();
    for header in &headers {
        if header.is_empty() || !unique.insert(header) {
            return Err(CoreError::invalid(format!("CSV {display} 表头为空或重复")));
        }
    }
    let mut values = Vec::new();
    let mut count = 0;
    for (index, row) in reader.records().enumerate() {
        let row = row.map_err(|error| {
            CoreError::invalid(format!("CSV {display} 第 {} 行: {error}", index + 2))
        })?;
        if row.len() != headers.len() {
            return Err(CoreError::invalid(format!(
                "CSV {display} 第 {} 行列数不符",
                index + 2
            )));
        }
        count += 1;
        enforce_limit(count, display)?;
        if collect {
            let object = headers
                .iter()
                .zip(row.iter())
                .map(|(key, value)| (key.to_owned(), Value::String(value.to_owned())))
                .collect::<Map<_, _>>();
            values.push(Value::Object(object));
        }
    }
    Ok((count, values))
}

fn read_jsonl(path: &Path, display: &str, collect: bool) -> Result<(usize, Vec<Value>), CoreError> {
    let reader = BufReader::new(File::open(path)?);
    let mut values = Vec::new();
    let mut count = 0;
    for (index, line) in reader.split(b'\n').enumerate() {
        let line = line.map_err(|error| {
            CoreError::invalid(format!("JSONL {display} 第 {} 行: {error}", index + 1))
        })?;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let line = std::str::from_utf8(&line).map_err(|_| {
            CoreError::invalid(format!("JSONL {display} 第 {} 行不是 UTF-8", index + 1))
        })?;
        let value: Value = serde_json::from_str(line).map_err(|error| {
            CoreError::invalid(format!("JSONL {display} 第 {} 行: {error}", index + 1))
        })?;
        if !value.is_object() {
            return Err(CoreError::invalid(format!(
                "JSONL {display} 第 {} 行不是对象",
                index + 1
            )));
        }
        count += 1;
        enforce_limit(count, display)?;
        if collect {
            values.push(value);
        }
    }
    Ok((count, values))
}

fn enforce_limit(records: usize, display: &str) -> Result<(), CoreError> {
    if records > MAX_RECORDS {
        Err(CoreError::invalid(format!(
            "数据源 {display} 超过 10000 条"
        )))
    } else {
        Ok(())
    }
}
