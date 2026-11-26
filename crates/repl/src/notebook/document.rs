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
struct RawNotebook<'a, Cells> {
    nbformat: u32,
    nbformat_minor: u32,
    #[serde(borrow)]
    metadata: &'a RawValue,
    cells: Cells,
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

impl<'a, Cells: Deserialize<'a>> RawNotebook<'a, Cells> {
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
        Ok(notebook)
    }
}

impl RawNotebook<'_, Vec<RawCell<'_>>> {
    fn validate_cells(&self) -> Result<()> {
        let mut ids = HashSet::default();
        for cell in &self.cells {
            if let Some(id) = &cell.id {
                ensure!(ids.insert(id), "Duplicate notebook cell ID: {id}");
            } else {
                ensure!(self.nbformat_minor < 5, "Missing notebook cell ID");
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
        Ok(())
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

fn raw_range(text: &str, value: &RawValue) -> Result<Range<usize>> {
    text.substr_range(value.get())
        .map(Into::into)
        .context("JSON value is outside the notebook")
}

#[derive(Clone, Copy)]
pub(super) enum CellChange {
    InsertCode,
    InsertMarkdown,
    Delete,
    MoveUp,
    MoveDown,
}

pub(super) fn structure_edit(
    text: &str,
    selected: usize,
    change: CellChange,
) -> Result<Option<(Range<usize>, String)>> {
    let notebook = RawNotebook::<&RawValue>::parse(text)?;
    let cells: Vec<&RawValue> = serde_json::from_str(notebook.cells.get())?;
    let cell = cells
        .get(selected)
        .map(|cell| raw_range(text, cell))
        .transpose()?;
    if cell.is_none() && !cells.is_empty() {
        return Ok(None);
    }
    let previous = selected.checked_sub(1).and_then(|index| cells.get(index));
    let next = cells.get(selected.saturating_add(1));
    let edit = match change {
        CellChange::InsertCode | CellChange::InsertMarkdown => {
            let mut new_cell = if matches!(change, CellChange::InsertCode) {
                serde_json::json!({"cell_type":"code", "metadata":{}, "source":[], "outputs":[], "execution_count":null})
            } else {
                serde_json::json!({"cell_type":"markdown", "metadata":{}, "source":[]})
            };
            if notebook.nbformat_minor == 5 {
                new_cell["id"] = uuid::Uuid::new_v4().to_string().into();
            }
            let start = raw_range(text, notebook.cells)?.start + 1;
            if let Some(cell) = cell {
                let separator = if let Some(next) = next {
                    &text[cell.end..raw_range(text, next)?.start]
                } else if let Some(previous) = previous {
                    &text[raw_range(text, previous)?.end..cell.start]
                } else {
                    &format!(",{}", &text[start..cell.start])
                };
                (cell.end..cell.end, format!("{separator}{new_cell}"))
            } else {
                (start..start, new_cell.to_string())
            }
        }
        CellChange::Delete => {
            let Some(mut range) = cell else {
                return Ok(None);
            };
            if let Some(next) = next {
                range.end = raw_range(text, next)?.start;
            } else if let Some(previous) = previous {
                range.start = raw_range(text, previous)?.end;
            }
            (range, String::new())
        }
        CellChange::MoveUp | CellChange::MoveDown => {
            let Some(cell) = cell else {
                return Ok(None);
            };
            let (first, second) = match (change, previous, next) {
                (CellChange::MoveUp, Some(previous), _) => (raw_range(text, previous)?, cell),
                (CellChange::MoveDown, _, Some(next)) => (cell, raw_range(text, next)?),
                _ => return Ok(None),
            };
            let range = first.start..second.end;
            let separator = &text[first.end..second.start];
            (range, [&text[second], separator, &text[first]].concat())
        }
    };
    Ok(Some(edit))
}

pub(super) fn parse_view(text: &str) -> Result<ParsedNotebook> {
    let raw = RawNotebook::parse(text)?;
    raw.validate_cells()?;
    let notebook = super::notebook_ui::upgrade_notebook(nbformat::parse_notebook(text)?)?;
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
                let outputs = raw_range(text, saved_outputs)?;
                let count = raw_range(text, count)?;
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
            source: raw_range(text, raw.source)?,
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

    fn change_cell(text: &str, index: usize, change: CellChange) -> Result<String> {
        Ok(apply(
            text,
            vec![structure_edit(text, index, change)?.context("expected a cell edit")?],
        ))
    }

    #[test]
    fn moving_and_deleting_cells_preserves_raw_objects_and_separators() -> Result<()> {
        use CellChange::*;
        let prefix = r#"{ "metadata":{"π":"[{}]", "cells":[]}, "nbformat_minor":4, "nbformat":4, "cells" : [  "#;
        let suffix = "\r\n ] }\n";
        let markdown =
            r#"{"source":"猫", "attachments":{"a":{}}, "metadata":{}, "cell_type":"markdown"}"#;
        let code = r#"{"cell_type":"code","execution_count":3,"outputs":[{"output_type":"stream","name":"stdout","text":["\u03c0\n"]}],"metadata":{"x":1.20e-7},"source":["print('π')\n"]}"#;
        let raw = r#"{ "cell_type":"raw", "metadata":{}, "source":"]}," }"#;
        let left = " \t,\r\n  ";
        let right = ",\n\t";
        let original = format!("{prefix}{markdown}{left}{code}{right}{raw}{suffix}");

        for (index, change, expected) in [
            (0, MoveDown, format!("{code}{left}{markdown}{right}{raw}")),
            (1, MoveUp, format!("{code}{left}{markdown}{right}{raw}")),
            (1, MoveDown, format!("{markdown}{left}{raw}{right}{code}")),
            (2, MoveUp, format!("{markdown}{left}{raw}{right}{code}")),
            (0, Delete, format!("{code}{right}{raw}")),
            (1, Delete, format!("{markdown}{left}{raw}")),
            (2, Delete, format!("{markdown}{left}{code}")),
        ] {
            assert_eq!(
                change_cell(&original, index, change)?,
                format!("{prefix}{expected}{suffix}")
            );
        }
        for (index, change) in [(0, MoveUp), (2, MoveDown), (3, Delete), (3, InsertCode)] {
            assert!(structure_edit(&original, index, change)?.is_none());
        }
        let single = format!("{prefix}{markdown}{suffix}");
        let empty = format!("{prefix}{suffix}");
        assert_eq!(change_cell(&single, 0, Delete)?, empty);
        for text in [&single, &empty] {
            for change in [MoveUp, MoveDown] {
                assert!(structure_edit(text, 0, change)?.is_none());
            }
        }
        assert!(structure_edit(&empty, 0, Delete)?.is_none());
        Ok(())
    }

    #[test]
    fn inserted_cells_have_the_right_shape_and_leave_existing_bytes_intact() -> Result<()> {
        for index in 0..3 {
            for (change, expected) in [
                (
                    CellChange::InsertCode,
                    json!({"cell_type":"code", "source":[], "metadata":{}, "outputs":[], "execution_count":null}),
                ),
                (
                    CellChange::InsertMarkdown,
                    json!({"cell_type":"markdown", "source":[], "metadata":{}}),
                ),
            ] {
                let saved = change_cell(NOTEBOOK, index, change)?;
                let cells: Value = serde_json::from_str(&saved)?;
                assert_eq!(cells["cells"][index + 1], expected);
                assert_eq!(parse_view(&saved)?.notebook.cells.len(), 4);
                assert_eq!(
                    change_cell(&saved, index + 1, CellChange::Delete)?,
                    NOTEBOOK
                );
            }
        }

        let empty = r#"{ "cells" : [  ], "metadata":{}, "nbformat":4, "nbformat_minor":5 }"#;
        let one = change_cell(empty, 0, CellChange::InsertMarkdown)?;
        let two = change_cell(&one, 0, CellChange::InsertCode)?;
        assert_eq!(parse_view(&two)?.notebook.cells.len(), 2);
        let value: Value = serde_json::from_str(&two)?;
        let markdown = &value["cells"][0];
        let code = &value["cells"][1];
        assert_eq!(markdown["cell_type"], "markdown");
        assert_eq!(code["cell_type"], "code");
        assert!(markdown["id"].is_string() && code["id"].is_string());
        assert_ne!(markdown["id"], code["id"]);
        assert_eq!(change_cell(&two, 1, CellChange::Delete)?, one);
        assert_eq!(change_cell(&one, 0, CellChange::Delete)?, empty);
        Ok(())
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
