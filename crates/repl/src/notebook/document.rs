use std::ops::Range;

use anyhow::{Context as _, Result, ensure};
use collections::HashSet;
use nbformat::v4::CellId;
use serde::{Deserialize, Deserializer, de::IgnoredAny};
use serde_json::{Value, value::RawValue};

pub(super) struct ParsedNotebook {
    pub notebook: nbformat::v4::Notebook,
    pub fields: Vec<CellFields<usize>>,
}

pub(super) struct CellFields<T> {
    pub source: Range<T>,
    pub multiline: bool,
    pub execution: Option<ExecutionFields<T>>,
    pub(super) last_source: String,
}

pub(super) struct ExecutionFields<T> {
    pub outputs: Range<T>,
    pub count: Range<T>,
    outputs_first: bool,
}

#[derive(Deserialize)]
struct RawNotebook<'a> {
    nbformat: u32,
    nbformat_minor: u32,
    metadata: &'a RawValue,
    #[serde(borrow)]
    cells: Vec<RawCell<'a>>,
}

#[derive(Deserialize)]
struct RawCell<'a> {
    id: Option<CellId>,
    cell_type: CellKind,
    metadata: &'a RawValue,
    source: &'a RawValue,
    #[serde(borrow)]
    outputs: Option<&'a RawValue>,
    #[serde(default, borrow, deserialize_with = "present_value")]
    execution_count: Option<&'a RawValue>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CellKind {
    Code,
    Markdown,
    Raw,
}

fn present_value<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<&'de RawValue>, D::Error> {
    <&RawValue>::deserialize(deserializer).map(Some)
}

impl<'a> RawNotebook<'a> {
    fn parse(text: &'a str) -> Result<Self> {
        let notebook: Self = serde_json::from_str(text)?;
        ensure!(
            notebook.nbformat == 4 && notebook.nbformat_minor <= 5,
            "Unsupported notebook version {}.{}",
            notebook.nbformat,
            notebook.nbformat_minor
        );
        ensure!(
            notebook.metadata.get().starts_with('{'),
            "Invalid notebook metadata"
        );
        let mut ids = HashSet::default();
        for cell in &notebook.cells {
            if let Some(id) = &cell.id {
                ensure!(ids.insert(id), "Duplicate notebook cell ID: {id}");
            } else {
                ensure!(notebook.nbformat_minor < 5, "Missing notebook cell ID");
            }
            ensure!(
                cell.metadata.get().starts_with('{'),
                "Invalid notebook cell metadata"
            );
            ensure!(
                cell.source.get().starts_with(['"', '[']),
                "Invalid notebook cell source"
            );
            if matches!(cell.cell_type, CellKind::Code) {
                let (outputs, count) = cell.execution_fields()?;
                ensure!(
                    outputs.get().starts_with('['),
                    "Invalid notebook cell outputs"
                );
                let count: Option<i32> = serde_json::from_str(count.get())?;
                ensure!(
                    count.is_none_or(|count| count >= 0),
                    "Invalid notebook execution count"
                );
            }
        }
        Ok(notebook)
    }
}

impl<'a> RawCell<'a> {
    fn execution_fields(&self) -> Result<(&'a RawValue, &'a RawValue)> {
        ensure!(
            matches!(self.cell_type, CellKind::Code),
            "Only code cells can run"
        );
        Ok((
            self.outputs.context("Code cell is missing outputs")?,
            self.execution_count
                .context("Code cell is missing an execution count")?,
        ))
    }
}

pub(super) fn parse_view(text: &str) -> Result<ParsedNotebook> {
    let raw = RawNotebook::parse(text)?;
    let notebook = super::notebook_ui::upgrade_notebook(nbformat::parse_notebook(text)?)?;
    let field_range = |value: &RawValue| -> Result<Range<usize>> {
        text.substr_range(value.get())
            .map(Into::into)
            .context("Cell field is outside the notebook")
    };
    let mut fields = Vec::with_capacity(raw.cells.len());
    for (raw, cell) in raw.cells.into_iter().zip(&notebook.cells) {
        let (source, execution) = match cell {
            nbformat::v4::Cell::Code {
                source, outputs, ..
            } => {
                let (saved_outputs, count) = raw.execution_fields()?;
                let output_count =
                    serde_json::from_str::<Vec<IgnoredAny>>(saved_outputs.get())?.len();
                ensure!(
                    output_count == outputs.len(),
                    "Unsupported notebook cell output"
                );
                let outputs = field_range(saved_outputs)?;
                let count = field_range(count)?;
                (
                    source,
                    Some(ExecutionFields {
                        outputs_first: outputs.start < count.start,
                        outputs,
                        count,
                    }),
                )
            }
            nbformat::v4::Cell::Markdown { source, .. }
            | nbformat::v4::Cell::Raw { source, .. } => (source, None),
        };
        fields.push(CellFields {
            source: field_range(raw.source)?,
            multiline: raw.source.get().starts_with('['),
            execution,
            last_source: source.concat(),
        });
    }
    Ok(ParsedNotebook { notebook, fields })
}

impl<T> CellFields<T> {
    pub(super) fn map_offsets<U>(self, mut map: impl FnMut(Range<T>) -> Range<U>) -> CellFields<U> {
        CellFields {
            source: map(self.source),
            multiline: self.multiline,
            execution: self.execution.map(|fields| ExecutionFields {
                outputs: map(fields.outputs),
                count: map(fields.count),
                outputs_first: fields.outputs_first,
            }),
            last_source: self.last_source,
        }
    }
}

impl<T: Clone> CellFields<T> {
    pub(super) fn edit_source(&mut self, source: &str) -> Result<Vec<(Range<T>, String)>> {
        if self.last_source == source {
            return Ok(Vec::new());
        }
        let replacement = if self.multiline {
            serde_json::to_string(&source.split_inclusive('\n').collect::<Vec<_>>())?
        } else {
            serde_json::to_string(source)?
        };
        source.clone_into(&mut self.last_source);
        Ok(vec![(self.source.clone(), replacement)])
    }

    pub(super) fn edit_execution(
        &self,
        outputs: &[Value],
        execution_count: Option<i64>,
    ) -> Result<Vec<(Range<T>, String)>> {
        let fields = self.execution.as_ref().context("Only code cells can run")?;
        ensure!(
            execution_count.is_none_or(|count| (0..=i64::from(i32::MAX)).contains(&count)),
            "Unsupported notebook execution count"
        );
        let outputs = (fields.outputs.clone(), serde_json::to_string(outputs)?);
        let count = (
            fields.count.clone(),
            serde_json::to_string(&execution_count)?,
        );
        Ok(if fields.outputs_first {
            vec![outputs, count]
        } else {
            vec![count, outputs]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOTEBOOK: &str = r#"{
  "nbformat":4, "nbformat_minor":4, "metadata": {"extra": 987654321012345678909876543210},
  "cells": [
    {"cell_type":"markdown","source":"same","metadata":{},"attachments":{"plot":{"image/png":"abc"}}},
    {"cell_type":"code","source":[ "sa", "\u006de" ],"metadata":{},"extra":1.230000e+07,
     "outputs" : [ {"output_type":"stream","name":"stdout","text":["old\n"]} ], "execution_count":9},
    {"cell_type":"code","metadata":{},"execution_count":9,"source":"same","outputs":[]}
  ]
}
"#;

    fn apply(text: &str, edits: Vec<(Range<usize>, String)>) -> String {
        let mut result = text.to_owned();
        for (range, replacement) in edits.into_iter().rev() {
            result.replace_range(range, &replacement);
        }
        result
    }

    #[test]
    fn source_edits_keep_saved_outputs_and_unrelated_bytes() -> Result<()> {
        let parsed = parse_view(NOTEBOOK)?;
        assert_eq!(parsed.notebook.cells.len(), 3);
        let mut fields = parsed.fields.into_iter();
        let mut markdown = fields.next().context("missing markdown cell")?;
        let mut code = fields.next().context("missing code cell")?;
        assert!(code.edit_source("same")?.is_empty());

        let source = "print('π')\r\n\nend";
        let saved = apply(NOTEBOOK, code.edit_source(source)?);
        assert!(code.edit_source(source)?.is_empty());
        assert_eq!(
            saved,
            NOTEBOOK.replace(r#"[ "sa", "\u006de" ]"#, r#"["print('π')\r\n","\n","end"]"#)
        );
        let reparsed = parse_view(&saved)?;
        assert!(
            matches!(reparsed.notebook.cells.get(1), Some(nbformat::v4::Cell::Code { source: lines, .. }) if lines.concat() == source)
        );
        assert_eq!(
            apply(NOTEBOOK, markdown.edit_source("new\n")?),
            NOTEBOOK.replacen(r#""source":"same""#, r#""source":"new\n""#, 1)
        );
        Ok(())
    }

    #[test]
    fn rerunning_replaces_only_the_executed_cells_results() -> Result<()> {
        let parsed = parse_view(NOTEBOOK)?;
        let mut fields = parsed.fields.into_iter();
        let markdown = fields.next().context("missing markdown cell")?;
        let first = fields.next().context("missing first code cell")?;
        let second = fields.next().context("missing second code cell")?;
        let outputs = [json!({"output_type":"stream","name":"stderr","text":"new\n"})];
        let saved = apply(NOTEBOOK, first.edit_execution(&outputs, Some(10))?);
        assert_eq!(
            saved,
            NOTEBOOK
                .replace(
                    r#"[ {"output_type":"stream","name":"stdout","text":["old\n"]} ]"#,
                    &serde_json::to_string(&outputs)?
                )
                .replacen(r#""execution_count":9"#, r#""execution_count":10"#, 1)
        );
        assert!(markdown.edit_execution(&outputs, None).is_err());
        assert!(fields.next().is_none());
        let second = second.map_offsets(|range| range.start + 1..range.end + 1);
        let saved = apply(
            &format!(" {NOTEBOOK}"),
            second.edit_execution(&outputs, None)?,
        );
        let parsed = parse_view(&saved)?;
        assert!(
            matches!(parsed.notebook.cells.last(), Some(nbformat::v4::Cell::Code { outputs, execution_count: None, .. }) if outputs.len() == 1)
        );
        Ok(())
    }

    #[test]
    fn unsupported_notebooks_are_not_editable() {
        for text in [
            NOTEBOOK.replace(r#""nbformat":4"#, r#""nbformat":3"#),
            NOTEBOOK.replace(r#""nbformat_minor":4"#, r#""nbformat_minor":6"#),
            NOTEBOOK.replace(r#""nbformat_minor":4"#, r#""nbformat_minor":5"#),
            NOTEBOOK.replace(
                r#""cell_type":"code""#,
                r#""cell_type":"code","id":"repeated""#,
            ),
            NOTEBOOK.replace(r#""cell_type":"markdown""#, r#""cell_type":"drawing""#),
            NOTEBOOK.replace(r#", "execution_count":9"#, ""),
        ] {
            assert!(parse_view(&text).is_err());
        }
        let unsupported_output =
            NOTEBOOK.replace(r#""output_type":"stream""#, r#""output_type":"unknown""#);
        assert!(parse_view(&unsupported_output).is_err());
    }
}
