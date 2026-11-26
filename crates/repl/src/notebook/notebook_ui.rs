use std::{future::Future, sync::Arc};

use super::{
    Cell, CellEvent, CellPosition, CodeCell, MarkdownCellEvent, RenderableCell, RunnableCell,
    document::{CellFields, ParsedNotebook},
};
use crate::kernels::{Kernel, KernelSession, LocalKernelSpecification, NativeRunningKernel};
use crate::notebook::MovementDirection;
use anyhow::{Context as _, Result};
use collections::HashMap;
use futures::{FutureExt, future::Shared};
use gpui::{App, Entity, FocusHandle, Focusable, KeyContext, ListState, Task, list, prelude::*};
use jupyter_protocol::{ExecuteRequest, JupyterMessage, JupyterMessageContent};
use language::{Language, LanguageRegistry};
use nbformat::v4::CellId;
use project::{
    Project, ProjectEntryId, ProjectPath,
    trusted_worktrees::{TrustedWorktrees, TrustedWorktreesEvent},
};
use ui::{Tooltip, prelude::*};
use workspace::item::SaveOptions;
use zed_actions::editor::{MoveDown, MoveUp};
use zed_actions::notebook::{
    EnterCommandMode, EnterEditMode, InterruptKernel, NotebookMoveDown, NotebookMoveUp,
    RestartKernel, Run, RunAndAdvance,
};

/// Whether the notebook is in command mode (navigating cells) or edit mode (editing a cell).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotebookMode {
    Command,
    Edit,
}

#[derive(PartialEq, Eq)]
enum SelectionMode {
    SelectOnly,
    SelectAndMove,
}

pub(crate) const MEDIUM_SPACING_SIZE: f32 = 12.0;
pub(crate) const GUTTER_WIDTH: f32 = 19.0;
pub(crate) const CODE_BLOCK_INSET: f32 = MEDIUM_SPACING_SIZE;

const NOTEBOOK_EXTENSION: &str = "ipynb";

pub fn init(cx: &mut App) {
    super::preview::init(cx);
}

pub struct NotebookEditor {
    project: Entity<Project>,
    worktree_id: project::WorktreeId,
    focus_handle: FocusHandle,
    notebook_item: Entity<NotebookItem>,
    cell_list: ListState,
    notebook_mode: NotebookMode,
    selected_cell_index: usize,
    cell_order: Vec<CellId>,
    source_version: clock::Global,
    sync_errors: HashMap<usize, String>,
    cell_sources: HashMap<usize, CellSourceHistory>,
    cell_map: HashMap<CellId, Cell>,
    kernel: Kernel,
    kernel_specification: Option<LocalKernelSpecification>,
    kernel_session: Option<Entity<NotebookKernelSession>>,
    kernel_task: Option<Shared<Task<()>>>,
    kernel_notice: Option<String>,
    execution_requests: HashMap<String, CellExecution>,
    display_slots: HashMap<CellId, DisplaySlots>,
}

struct NotebookKernelSession(gpui::WeakEntity<NotebookEditor>);

struct CellSourceHistory {
    cell: Entity<language::Buffer>,
    observed: clock::Global,
    transactions: HashMap<clock::Lamport, language::Transaction>,
}

impl CellSourceHistory {
    fn has_pending_edits(&self, cx: &App) -> bool {
        self.cell.read(cx).as_text_snapshot().version() != &self.observed
    }

    fn sync(
        &mut self,
        operation: &text::Operation,
        fields: &mut CellFields<language::Anchor>,
        buffer: &mut language::Buffer,
        cx: &mut Context<language::Buffer>,
    ) -> Result<()> {
        let mut version = match operation {
            text::Operation::Edit(edit) => edit.version.clone(),
            text::Operation::Undo(undo) => undo.version.clone(),
        };
        version.observe(operation.timestamp());
        let cell = self.cell.read(cx);
        let text = cell.rope_for_version(&version);
        let source = text::chunks_with_line_ending(&text, cell.line_ending()).collect::<String>();

        match operation {
            text::Operation::Edit(edit) => {
                let edits = fields.edit_source(&source)?;
                if !edits.is_empty() {
                    buffer.finalize_last_transaction();
                    buffer.edit(edits, None, cx);
                    let transaction = buffer
                        .finalize_last_transaction()
                        .context("Missing notebook source transaction")?;
                    self.transactions
                        .insert(edit.timestamp, transaction.clone());
                    return Ok(());
                }
            }
            text::Operation::Undo(undo) => {
                let mut affected: Vec<_> = undo
                    .counts
                    .iter()
                    .filter_map(|(edit, count)| {
                        self.transactions
                            .get(edit)
                            .map(|transaction| (transaction, *count))
                    })
                    .collect();
                affected.sort_unstable_by_key(|(transaction, _)| transaction.id);
                for (transaction, _) in affected.iter().rev().filter(|(_, count)| count % 2 == 1) {
                    anyhow::ensure!(
                        buffer.undo_transaction(transaction.id, cx),
                        "Could not undo the notebook cell edit"
                    );
                }
                for (transaction, _) in affected.into_iter().filter(|(_, count)| count % 2 == 0) {
                    anyhow::ensure!(
                        buffer.redo_transaction(transaction.clone(), cx),
                        "Could not redo the notebook cell edit"
                    );
                    buffer.finalize_last_transaction();
                }
            }
        }
        let raw = buffer
            .text_for_range(fields.source.clone())
            .collect::<String>();
        let mut restored: String = if fields.multiline {
            serde_json::from_str::<Vec<String>>(&raw)?.concat()
        } else {
            serde_json::from_str(&raw)?
        };
        language::LineEnding::normalize(&mut restored);
        anyhow::ensure!(
            restored == text.to_string(),
            "Cell source history no longer matches the notebook"
        );
        fields.last_source = source;
        Ok(())
    }
}

struct CellRun {
    cell_id: CellId,
    source: String,
    advance: bool,
}

type DisplaySlots = Vec<(String, usize)>;

struct CellExecution {
    cell_id: CellId,
    source: String,
    outputs: Vec<serde_json::Value>,
    reply: Option<jupyter_protocol::ExecuteReply>,
    idle: bool,
    clear_on_output: bool,
    previous: Option<(super::cell::PreviousExecution, DisplaySlots)>,
}

impl CellExecution {
    fn new(cell_id: CellId, source: String) -> Self {
        Self {
            cell_id,
            source,
            outputs: Vec::new(),
            reply: None,
            idle: false,
            clear_on_output: false,
            previous: None,
        }
    }

    fn started(&self) -> bool {
        self.previous.is_some()
    }

    fn queued(cell: &Entity<CodeCell>, source: String, cx: &mut App) -> Self {
        cell.update(cx, |cell, cx| {
            cell.queue_execution();
            cx.notify();
            Self::new(cell.id().clone(), source)
        })
    }
}

impl NotebookEditor {
    pub fn new(
        project: Entity<Project>,
        notebook_item: Entity<NotebookItem>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        let languages = project.read(cx).languages().clone();
        let worktree_id = notebook_item.read(cx).project_path.worktree_id;

        let notebook_language = notebook_item.read(cx).notebook_language();
        let notebook_language = cx
            .spawn_in(window, async move |_, _| notebook_language.await)
            .shared();

        let cells = notebook_item.update(cx, |item, _| std::mem::take(&mut item.notebook.cells));
        let mut cell_order = Vec::with_capacity(cells.len());
        let mut cell_sources = HashMap::default();
        let mut cell_map = HashMap::default();
        for (index, cell) in cells.into_iter().enumerate() {
            let cell_id = cell.id().clone();
            cell_order.push(cell_id.clone());
            let cell_entity = Cell::load(&cell, &languages, notebook_language.clone(), window, cx);

            match &cell_entity {
                Cell::Code(code_cell) => {
                    let cell_id_for_focus = cell_id.clone();
                    cx.subscribe_in(code_cell, window, move |this, _cell, event, window, cx| {
                        match event {
                            CellEvent::Run(cell_id) => {
                                this.execute_cell(cell_id.clone(), false, window, cx);
                            }
                            CellEvent::FocusedIn(_) => {
                                this.select_cell_by_id(&cell_id_for_focus, cx)
                            }
                        }
                    })
                    .detach();
                }
                Cell::Markdown(markdown_cell) => {
                    cx.subscribe(markdown_cell, |_, cell, _: &MarkdownCellEvent, cx| {
                        cell.update(cx, |cell, cx| cell.reparse_markdown(cx));
                    })
                    .detach();
                }
                Cell::Raw(_) => {}
            }

            if let Some(editor) = cell_entity.editor(cx).cloned() {
                let focused_cell = cell_id.clone();
                cx.subscribe(&editor, move |this, _, event, cx| {
                    if matches!(event, editor::EditorEvent::Focused) {
                        this.select_cell_by_id(&focused_cell, cx);
                    }
                })
                .detach();
                if let Some(cell) = editor.read(cx).buffer().read(cx).as_singleton() {
                    cell_sources.insert(
                        index,
                        CellSourceHistory {
                            cell: cell.clone(),
                            observed: cell.read(cx).version(),
                            transactions: HashMap::default(),
                        },
                    );
                    cx.subscribe(&cell, move |this, _, event, cx| {
                        let language::BufferEvent::Operation {
                            operation: language::Operation::Buffer(operation),
                            ..
                        } = event
                        else {
                            return;
                        };
                        this.apply_cell_operation(index, operation, cx);
                    })
                    .detach();
                }
            }
            cell_map.insert(cell_id, cell_entity);
        }

        let cell_count = cell_order.len();

        let cell_list = ListState::new(cell_count, gpui::ListAlignment::Top, px(1000.));

        let mut editor = Self {
            project,
            worktree_id,
            focus_handle,
            notebook_item: notebook_item.clone(),
            cell_list,
            notebook_mode: NotebookMode::Command,
            selected_cell_index: 0,
            cell_order,
            source_version: notebook_item.read(cx).buffer.read(cx).version(),
            sync_errors: HashMap::default(),
            cell_sources,
            cell_map,
            kernel: Kernel::Shutdown,
            kernel_specification: None,
            execution_requests: HashMap::default(),
            display_slots: HashMap::default(),
            kernel_session: None,
            kernel_task: None,
            kernel_notice: None,
        };
        editor.watch_kernel_trust(cx);
        editor
    }

    fn check_execution(&self, cx: &mut App) -> Result<()> {
        let project = self.project.read(cx);
        anyhow::ensure!(
            project.is_local(),
            "Notebook execution requires a local project."
        );
        anyhow::ensure!(
            self.notebook_item
                .read(cx)
                .language_name()
                .is_none_or(|language| language.eq_ignore_ascii_case("python")),
            "Only Python notebooks can be executed."
        );
        let store = project.worktree_store();
        let trusted = TrustedWorktrees::try_get_global(cx).is_some_and(|trust| {
            !TrustedWorktrees::has_restricted_worktrees(&store, cx)
                && trust.update(cx, |trust, cx| {
                    trust.can_trust(&store, self.worktree_id, cx)
                })
        });
        anyhow::ensure!(
            trusted,
            "Notebook execution is disabled in Restricted Mode. Trust this workspace to run Python."
        );
        anyhow::ensure!(
            self.source_is_current(cx) && self.sync_errors.is_empty(),
            "The notebook source changed. Reopen the notebook before running it."
        );
        Ok(())
    }

    fn watch_kernel_trust(&mut self, cx: &mut Context<Self>) {
        if let Some(trust) = TrustedWorktrees::try_get_global(cx) {
            let store = self.project.read(cx).worktree_store().downgrade();
            cx.subscribe(&trust, move |this, _, event, cx| {
                if matches!(event, TrustedWorktreesEvent::Restricted(changed, _) if changed == &store) {
                    this.stop_kernel(cx);
                    this.kernel_specification = None;
                    this.kernel_notice = Some("Notebook execution is disabled in Restricted Mode.".into());
                    cx.notify();
                }
            }).detach();
        }
    }

    pub(super) fn stop_kernel(&mut self, cx: &mut Context<Self>) {
        self.kernel_task.take();
        self.kernel_session = None;
        self.kernel = Kernel::Shutdown;
        self.kernel_notice = (!self.execution_requests.is_empty())
            .then(|| "Kernel stopped. Your previous results are unchanged.".into());
        self.display_slots.clear();
        for (_, run) in self.execution_requests.drain() {
            if let Some(Cell::Code(cell)) = self.cell_map.get(&run.cell_id) {
                cell.update(cx, |cell, cx| {
                    cell.cancel_execution(run.previous.map(|(previous, _)| previous));
                    cx.notify();
                });
            }
        }
        cx.notify();
    }

    pub(super) fn source_is_current(&self, cx: &App) -> bool {
        self.notebook_item.read(cx).buffer.read(cx).version() == self.source_version
    }

    pub(super) fn project_path<'a>(&self, cx: &'a App) -> &'a ProjectPath {
        &self.notebook_item.read(cx).project_path
    }

    fn patch_cell(
        &mut self,
        cell_index: usize,
        cx: &mut Context<Self>,
        patch: impl FnOnce(
            &mut CellFields<language::Anchor>,
        ) -> Result<Vec<(std::ops::Range<language::Anchor>, String)>>,
    ) -> Result<()> {
        self.update_cell(cell_index, cx, |fields, _, buffer, cx| {
            buffer.edit(patch(fields)?, None, cx);
            Ok(())
        })
    }

    fn update_cell(
        &mut self,
        cell_index: usize,
        cx: &mut Context<Self>,
        update: impl FnOnce(
            &mut CellFields<language::Anchor>,
            &mut CellSourceHistory,
            &mut language::Buffer,
            &mut Context<language::Buffer>,
        ) -> Result<()>,
    ) -> Result<()> {
        let buffer = self.notebook_item.read(cx).buffer.clone();
        anyhow::ensure!(
            self.source_is_current(cx),
            "The notebook source changed. Reopen the notebook before editing its cells."
        );
        anyhow::ensure!(
            buffer.read(cx).capability().editable(),
            "The notebook source is read-only."
        );
        let history = self
            .cell_sources
            .get_mut(&cell_index)
            .context("Cell is not editable")?;
        let result = self.notebook_item.update(cx, |item, cx| {
            let fields = item
                .fields
                .get_mut(cell_index)
                .context("Cell no longer exists")?;
            buffer.update(cx, |buffer, cx| update(fields, history, buffer, cx))
        });
        self.source_version = buffer.read(cx).version();
        result
    }

    fn apply_cell_operation(
        &mut self,
        index: usize,
        operation: &text::Operation,
        cx: &mut Context<Self>,
    ) {
        if self
            .cell_sources
            .get(&index)
            .is_some_and(|history| history.observed.observed(operation.timestamp()))
        {
            return;
        }
        let result = self.update_cell(index, cx, |fields, history, buffer, cx| {
            history.observed.observe(operation.timestamp());
            history.sync(operation, fields, buffer, cx)
        });
        match result {
            Ok(()) => {
                self.sync_errors.remove(&index);
            }
            Err(error) => {
                self.sync_errors
                    .insert(index, format!("Cell {}: {error}", index + 1));
            }
        }
        cx.notify();
    }

    pub(super) fn flush_cell_edits(&mut self, cx: &mut Context<Self>) -> Result<()> {
        let mut pending = Vec::new();
        for (&index, history) in &self.cell_sources {
            if history.has_pending_edits(cx) {
                let cell = history.cell.read(cx);
                let mut version = history.observed.clone();
                for (id, operation) in cell
                    .operations()
                    .iter()
                    .filter(|(id, _)| !history.observed.observed(**id))
                {
                    pending.push((index, operation.clone()));
                    version.observe(*id);
                }
                anyhow::ensure!(
                    &version == cell.as_text_snapshot().version(),
                    "Cell {} has unmirrored edits",
                    index + 1
                );
            }
        }
        for (index, operation) in pending {
            self.apply_cell_operation(index, &operation, cx);
        }
        if let Some(error) = self.sync_errors.values().next() {
            anyhow::bail!("{error}");
        }
        Ok(())
    }

    pub(super) fn has_unsynced_edits(&self, cx: &App) -> bool {
        !self.sync_errors.is_empty()
            || self
                .cell_sources
                .values()
                .any(|history| history.has_pending_edits(cx))
    }

    fn persist_execution(
        &mut self,
        cell_index: usize,
        source: &str,
        outputs: &[serde_json::Value],
        execution_count: Option<i64>,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let cell = self
            .cell_order
            .get(cell_index)
            .and_then(|id| self.cell_map.get(id))
            .context("The executed cell no longer exists")?;
        anyhow::ensure!(
            cell.current_source(cx) == source,
            "The cell changed while running. Run it again to save its outputs."
        );
        self.patch_cell(cell_index, cx, |fields| {
            fields.edit_execution(outputs, execution_count)
        })
    }

    pub(super) fn cell(&self, index: usize) -> Option<&Cell> {
        self.cell_order
            .get(index)
            .and_then(|id| self.cell_map.get(id))
    }

    pub(super) fn save(
        &mut self,
        _options: SaveOptions,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if let Err(error) = self.flush_cell_edits(cx) {
            return Task::ready(Err(error));
        }
        let buffer = self.notebook_item.read(cx).buffer.clone();
        project.update(cx, |project, cx| project.save_buffer(buffer, cx))
    }

    fn project_python(
        &self,
        use_toolchain: bool,
        cx: &App,
    ) -> Result<Task<Option<std::path::PathBuf>>> {
        let project = self.project.read(cx);
        let path = self.notebook_item.read(cx).project_path.clone();
        let tree = project
            .worktree_for_id(path.worktree_id, cx)
            .context("Notebook worktree is no longer open")?;
        let tree = tree.read(cx);
        if tree.is_single_file() {
            return Ok(Task::ready(None));
        }
        let default_python = tree.abs_path().join(if cfg!(windows) {
            ".venv/Scripts/python.exe"
        } else {
            ".venv/bin/python"
        });
        if !use_toolchain {
            return Ok(Task::ready(Some(default_python)));
        }
        let toolchain =
            project.active_toolchain(path, language::LanguageName::new_static("Python"), cx);
        Ok(cx.background_spawn(async move {
            Some(
                toolchain
                    .await
                    .map(|toolchain| std::path::PathBuf::from(toolchain.path.as_ref()))
                    .unwrap_or(default_python),
            )
        }))
    }

    fn choose_or_start_kernel(
        &mut self,
        run: Option<CellRun>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = self.check_execution(cx) {
            self.kernel_notice = Some(error.to_string());
            cx.notify();
            return;
        }
        if self.kernel_task.is_some() {
            self.kernel_notice = Some("Cell not queued. Run it again after kernel setup.".into());
            cx.notify();
            return;
        }
        let selected = run.as_ref().and(self.kernel_specification.clone());
        let fs = self.project.read(cx).fs().clone();
        self.kernel_notice = Some("Finding a Python kernel…".into());
        let prepare = cx.spawn_in(window, async move |this, cx| {
            let result = Self::prepare_kernel(&this, selected, fs, run.as_ref(), cx).await;
            this.update_in(cx, |editor, window, cx| {
                editor.kernel_task = None;
                editor.kernel_notice = result.err().map(|error| {
                    error
                        .chain()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n")
                });
                if editor
                    .kernel_session
                    .take_if(|_| matches!(editor.kernel, Kernel::Shutdown))
                    .is_some()
                    && let Some(error) = &editor.kernel_notice
                    && let Some(run) = run
                    && let Some(Cell::Code(cell)) = editor.cell_map.get(&run.cell_id)
                {
                    cell.update(cx, |cell, cx| cell.show_kernel_error(error, window, cx));
                }
                cx.notify();
            })
            .ok();
        });
        self.kernel_task = Some(prepare.shared());
        cx.notify();
    }

    async fn prepare_kernel(
        this: &gpui::WeakEntity<Self>,
        selected: Option<LocalKernelSpecification>,
        fs: Arc<dyn project::Fs>,
        run: Option<&CellRun>,
        cx: &mut gpui::AsyncWindowContext,
    ) -> Result<()> {
        let check = |editor: &Self, cx: &mut App| -> Result<()> {
            editor.check_execution(cx)?;
            if let Some(run) = run {
                anyhow::ensure!(
                    editor
                        .cell_map
                        .get(&run.cell_id)
                        .is_some_and(|cell| cell.current_source(cx) == run.source),
                    "The cell changed. Run it again."
                );
            }
            Ok(())
        };
        let (mut specs, preferred) = if let Some(spec) = selected {
            (vec![spec], None)
        } else {
            let project_python = this
                .update(cx, |editor, cx| {
                    check(editor, cx)?;
                    editor.project_python(run.is_some(), cx)
                })??
                .await;
            let specs = cx
                .background_spawn(crate::kernels::notebook_python_kernels(
                    fs,
                    project_python.clone(),
                ))
                .await?;
            (specs, project_python)
        };
        let mut diagnostic = None;
        if run.is_some() {
            this.update(cx, |editor, cx| check(editor, cx))??;
            (specs, diagnostic) = cx
                .background_spawn(async move {
                    crate::kernels::suitable_notebook_python_kernels(specs, preferred.as_deref())
                        .await
                })
                .await;
        }
        this.update(cx, |editor, cx| check(editor, cx))??;
        let spec = if specs.is_empty() {
            anyhow::ensure!(
                run.is_some(),
                "No Python kernel found. Run a cell to set up Python for Zed."
            );
            let consent = this.update_in(cx, |editor, window, cx| {
                    let reason = if let Some(selected) = editor.kernel_specification.take() {
                        if let Some(error) = diagnostic {
                            if let Some(run) = run
                                && let Some(Cell::Code(cell)) = editor.cell_map.get(&run.cell_id) {
                                cell.update(cx, |cell, cx| cell.show_kernel_error(&error, window, cx));
                            }
                            error
                        } else {
                            format!("ipykernel is not installed in {} ({}).", selected.name, selected.path.display())
                        }
                    } else {
                        diagnostic.unwrap_or_else(|| "No local Python kernel was found.".into())
                    };
                    editor.kernel_notice = Some(reason.clone());
                    cx.notify();
                    window.prompt(gpui::PromptLevel::Info, "Set up Python for notebooks?",
                        Some(&format!("{reason}\n\nZed will download a private Python environment with ipykernel and matplotlib. This won't change your project's Python environment.")),
                        &["Set Up Python", "Cancel"], cx)
                })?;
            if consent.await.ok() != Some(0) {
                anyhow::bail!("Python setup was canceled. Run a cell to try again.");
            }
            let install = this.update(cx, |editor, cx| {
                check(editor, cx)?;
                editor.kernel_notice = Some("Setting up Python for notebooks…".into());
                cx.notify();
                anyhow::Ok(super::kernel_setup::install_python_kernel(cx))
            })??;
            let path = install.await?;
            LocalKernelSpecification::for_notebook_python(path, "Zed Python".into())?
        } else if run.is_some() && specs.len() == 1 {
            specs.remove(0)
        } else {
            let mut buttons: Vec<_> = specs
                .iter()
                .map(|spec| {
                    gpui::PromptButton::new(format!("{} ({})", spec.name, spec.path.display()))
                })
                .collect();
            buttons.push(gpui::PromptButton::cancel("Cancel"));
            let choice = cx
                .update(|window, cx| {
                    window.prompt(
                        gpui::PromptLevel::Info,
                        "Select a local Python kernel",
                        Some("Selecting a kernel does not run a cell."),
                        &buttons,
                        cx,
                    )
                })?
                .await?;
            if let Some(spec) = specs.get(choice) {
                this.update(cx, |editor, cx| {
                    check(editor, cx)?;
                    editor.select_kernel(spec.clone(), cx);
                    anyhow::Ok(())
                })??;
            }
            return Ok(());
        };
        let Some(run) = run else { return Ok(()) };
        let launch = this.update_in(cx, |editor, window, cx| {
            check(editor, cx)?;
            let directory = editor
                .project
                .read(cx)
                .absolute_path(&editor.notebook_item.read(cx).project_path, cx)
                .and_then(|path| path.parent().map(ToOwned::to_owned))
                .context("Cannot find notebook directory")?;
            let owner = cx.weak_entity();
            let session = cx.new(|_| NotebookKernelSession(owner));
            editor.kernel_session = Some(session.clone());
            editor.kernel_specification = Some(spec.clone());
            editor.kernel_notice = Some(format!("Starting {}…", spec.name));
            cx.notify();
            Ok::<_, anyhow::Error>(NativeRunningKernel::new(
                spec,
                session.entity_id(),
                directory,
                editor.project.read(cx).fs().clone(),
                session,
                window,
                cx,
            ))
        })??;
        let kernel = launch.await.context("the kernel failed to launch")?;
        this.update_in(cx, |editor, window, cx| {
            check(editor, cx)?;
            editor.kernel = Kernel::RunningKernel(kernel);
            editor.dispatch_cell(run, window, cx)
        })??;
        Ok(())
    }

    fn select_kernel(&mut self, spec: LocalKernelSpecification, cx: &mut Context<Self>) {
        if self.kernel_specification.as_ref() != Some(&spec) {
            self.stop_kernel(cx);
            self.kernel_specification = Some(spec);
        }
        self.kernel_notice = None;
        cx.notify();
    }

    fn restart_kernel(&mut self, _: &RestartKernel, _window: &mut Window, cx: &mut Context<Self>) {
        self.stop_kernel(cx);
        self.kernel_notice
            .get_or_insert_with(|| "Kernel stopped. Run a cell to start a new session.".into());
    }

    fn interrupt_kernel(
        &mut self,
        _: &InterruptKernel,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Kernel::RunningKernel(kernel) = &self.kernel
            && let Err(error) = kernel
                .request_tx()
                .try_send(jupyter_protocol::InterruptRequest {}.into())
        {
            self.kernel_notice = Some(format!("Could not interrupt the kernel: {error}"));
            cx.notify();
        }
    }

    fn execute_cell(
        &mut self,
        cell_id: CellId,
        advance: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Cell::Code(cell)) = self.cell_map.get(&cell_id) else {
            return;
        };
        let run = CellRun {
            cell_id,
            source: cell.read(cx).current_source(cx),
            advance,
        };
        if !matches!(self.kernel, Kernel::RunningKernel(_)) {
            self.choose_or_start_kernel(Some(run), window, cx);
            return;
        }
        let result = self.dispatch_cell(&run, window, cx);
        self.kernel_notice = result.err().map(|error| error.to_string());
        cx.notify();
    }

    fn dispatch_cell(
        &mut self,
        run: &CellRun,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        self.check_execution(cx)?;
        anyhow::ensure!(
            !self
                .execution_requests
                .values()
                .any(|execution| execution.cell_id == run.cell_id),
            "This cell is already running."
        );
        let Some(Cell::Code(cell)) = self.cell_map.get(&run.cell_id) else {
            anyhow::bail!("Cell not found");
        };
        let Kernel::RunningKernel(kernel) = &self.kernel else {
            anyhow::bail!("Kernel is not running");
        };
        let message: JupyterMessage = ExecuteRequest {
            code: run.source.clone(),
            ..Default::default()
        }
        .into();
        let id = message.header.msg_id.clone();
        kernel
            .request_tx()
            .try_send(message)
            .map_err(|error| anyhow::anyhow!("Could not send code to the kernel: {error}"))?;
        self.execution_requests
            .insert(id, CellExecution::queued(cell, run.source.clone(), cx));
        if run.advance && self.cell_order.get(self.selected_cell_index) == Some(&run.cell_id) {
            self.advance_in_command_mode(window, cx);
        }
        cx.notify();
        Ok(())
    }

    fn get_selected_cell(&self) -> Option<&Cell> {
        self.cell_order
            .get(self.selected_cell_index)
            .and_then(|cell_id| self.cell_map.get(cell_id))
    }

    fn run_current_cell(&mut self, _: &Run, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cell_id) = self.cell_order.get(self.selected_cell_index).cloned() else {
            return;
        };
        let Some(cell) = self.cell_map.get(&cell_id) else {
            return;
        };
        match cell {
            Cell::Code(_) => {
                self.execute_cell(cell_id, false, window, cx);
            }
            Cell::Markdown(markdown_cell) => {
                // for markdown, finish editing and move to next cell
                let is_editing = markdown_cell.read(cx).is_editing();
                if is_editing {
                    markdown_cell.update(cx, |cell, cx| {
                        cell.run(cx);
                    });
                    self.enter_command_mode(window, cx);
                }
            }
            Cell::Raw(_) => {}
        }
    }

    fn run_and_advance(&mut self, _: &RunAndAdvance, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(cell_id) = self.cell_order.get(self.selected_cell_index).cloned() {
            if let Some(cell) = self.cell_map.get(&cell_id) {
                match cell {
                    Cell::Code(_) => {
                        self.execute_cell(cell_id, true, window, cx);
                        return;
                    }
                    Cell::Markdown(markdown_cell) => {
                        if markdown_cell.read(cx).is_editing() {
                            markdown_cell.update(cx, |cell, cx| {
                                cell.run(cx);
                            });
                        }
                    }
                    Cell::Raw(_) => {}
                }
            }
        }

        self.advance_in_command_mode(window, cx);
    }

    fn enter_edit_mode(&mut self, _: &EnterEditMode, window: &mut Window, cx: &mut Context<Self>) {
        self.notebook_mode = NotebookMode::Edit;
        if let Some(cell_id) = self.cell_order.get(self.selected_cell_index) {
            if let Some(cell) = self.cell_map.get(cell_id) {
                match cell {
                    Cell::Code(code_cell) => {
                        let editor = code_cell.read(cx).editor().clone();
                        window.focus(&editor.focus_handle(cx), cx);
                    }
                    Cell::Markdown(markdown_cell) => {
                        markdown_cell.update(cx, |cell, cx| {
                            cell.set_editing(true);
                            cx.notify();
                        });
                        let editor = markdown_cell.read(cx).editor().clone();
                        window.focus(&editor.focus_handle(cx), cx);
                    }
                    Cell::Raw(_) => {}
                }
            }
        }
        cx.notify();
    }

    fn enter_command_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.notebook_mode = NotebookMode::Command;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn handle_enter_command_mode(
        &mut self,
        _: &EnterCommandMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.enter_command_mode(window, cx);
    }

    /// Advances to the next cell while staying in command mode (used by RunAndAdvance and shift-enter).
    fn advance_in_command_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.cell_count();
        if count == 0 {
            return;
        }
        if self.selected_cell_index < count - 1 {
            self.selected_cell_index += 1;
            self.cell_list
                .scroll_to_reveal_item(self.selected_cell_index);
        }
        self.notebook_mode = NotebookMode::Command;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn cell_count(&self) -> usize {
        self.cell_map.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_cell_index
    }

    fn select_cell_by_id(&mut self, cell_id: &CellId, cx: &mut Context<Self>) {
        if let Some(index) = self.cell_order.iter().position(|id| id == cell_id) {
            self.selected_cell_index = index;
            self.notebook_mode = NotebookMode::Edit;
            cx.notify();
        }
    }

    pub fn set_selected_index(
        &mut self,
        index: usize,
        jump_to_index: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // let previous_index = self.selected_cell_index;
        self.selected_cell_index = index;
        let current_index = self.selected_cell_index;

        // in the future we may have some `on_cell_change` event that we want to fire here

        if jump_to_index {
            self.jump_to_cell(current_index, window, cx);
        }
    }

    fn select_next(
        &mut self,
        _: &menu::SelectNext,
        selection_mode: SelectionMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.cell_count();
        if count > 0 {
            let index = self.selected_index();
            let ix = if index == count - 1 {
                count - 1
            } else {
                index + 1
            };
            self.set_selected_index(ix, true, window, cx);

            if selection_mode == SelectionMode::SelectAndMove
                && let Some(cell) = self.get_selected_cell()
            {
                cell.move_to(MovementDirection::Start, window, cx);
            }

            cx.notify();
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        selection_mode: SelectionMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.cell_count();
        if count > 0 {
            let index = self.selected_index();
            let ix = if index == 0 { 0 } else { index - 1 };
            self.set_selected_index(ix, true, window, cx);

            if selection_mode == SelectionMode::SelectAndMove
                && let Some(cell) = self.get_selected_cell()
            {
                cell.move_to(MovementDirection::End, window, cx);
            }

            cx.notify();
        }
    }

    pub fn select_first(
        &mut self,
        _: &menu::SelectFirst,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.cell_count();
        if count > 0 {
            self.set_selected_index(0, true, window, cx);
            cx.notify();
        }
    }

    pub fn select_last(
        &mut self,
        _: &menu::SelectLast,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.cell_count();
        if count > 0 {
            self.set_selected_index(count - 1, true, window, cx);
            cx.notify();
        }
    }

    fn jump_to_cell(&mut self, index: usize, _window: &mut Window, _cx: &mut Context<Self>) {
        self.cell_list.scroll_to_reveal_item(index);
    }

    fn render_kernel_status_bar(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status = self.kernel.status();
        let name = self
            .kernel_specification
            .as_ref()
            .map(|spec| spec.name.as_str())
            .unwrap_or("Select Kernel");
        let notice = self
            .kernel_notice
            .clone()
            .unwrap_or_else(|| match &self.kernel {
                Kernel::Shutdown => "Not running".into(),
                Kernel::ErroredLaunch(error) => error.clone(),
                _ => status.to_string(),
            });
        h_flex()
            .w_full()
            .px_3()
            .py_1()
            .gap_2()
            .bg(cx.theme().colors().status_bar_background)
            .child(
                Button::new("kernel-selector", name.to_owned())
                    .label_size(LabelSize::Small)
                    .disabled(self.kernel_task.is_some())
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.choose_or_start_kernel(None, window, cx);
                    })),
            )
            .child(
                div()
                    .id("kernel-status")
                    .child(
                        Label::new(notice.lines().next().unwrap_or_default().to_owned())
                            .size(LabelSize::Small),
                    )
                    .when(notice.contains('\n'), |this| {
                        this.tooltip(Tooltip::text(notice))
                    }),
            )
            .when(matches!(self.kernel, Kernel::RunningKernel(_)), |bar| {
                bar.child(
                    IconButton::new("stop-kernel", IconName::Power)
                        .tooltip(Tooltip::text("Stop Kernel"))
                        .on_click(cx.listener(|this, _, _, cx| this.stop_kernel(cx))),
                )
                .child(
                    IconButton::new("interrupt-kernel", IconName::Stop)
                        .disabled(self.execution_requests.is_empty())
                        .tooltip(|_, cx| {
                            Tooltip::for_action("Interrupt Kernel", &InterruptKernel, cx)
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.interrupt_kernel(&InterruptKernel, window, cx)
                        })),
                )
            })
    }

    fn cell_list(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        list(self.cell_list.clone(), move |index, _, cx| {
            view.update(cx, |this, cx| match this.cell(index) {
                Some(cell) => this.render_cell(index, cell, cx).into_any_element(),
                None => div().into_any_element(),
            })
        })
        .size_full()
    }

    fn cell_position(&self, index: usize) -> CellPosition {
        match index {
            0 => CellPosition::First,
            index if index == self.cell_count() - 1 => CellPosition::Last,
            _ => CellPosition::Middle,
        }
    }

    fn render_cell(&self, index: usize, cell: &Cell, cx: &mut Context<Self>) -> impl IntoElement {
        let cell_position = self.cell_position(index);

        let is_selected = index == self.selected_cell_index;

        match cell {
            Cell::Code(cell) => {
                cell.update(cx, |cell, _cx| {
                    cell.set_selected(is_selected)
                        .set_cell_position(cell_position);
                });
                cell.clone().into_any_element()
            }
            Cell::Markdown(cell) => {
                cell.update(cx, |cell, _cx| {
                    cell.set_selected(is_selected)
                        .set_cell_position(cell_position);
                });
                cell.clone().into_any_element()
            }
            Cell::Raw(cell) => {
                cell.update(cx, |cell, _cx| {
                    cell.set_selected(is_selected)
                        .set_cell_position(cell_position);
                });
                cell.clone().into_any_element()
            }
        }
    }
}

impl Render for NotebookEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut key_context = KeyContext::new_with_defaults(cx.platform_info().os);
        key_context.add("NotebookEditor");
        key_context.set(
            "notebook_mode",
            match self.notebook_mode {
                NotebookMode::Command => "command",
                NotebookMode::Edit => "edit",
            },
        );

        v_flex()
            .size_full()
            .key_context(key_context)
            .track_focus(&self.focus_handle)
            .on_action(
                cx.listener(|this, _: &Run, window, cx| this.run_current_cell(&Run, window, cx)),
            )
            .on_action(
                cx.listener(|this, action, window, cx| this.run_and_advance(action, window, cx)),
            )
            .on_action(
                cx.listener(|this, action, window, cx| this.enter_edit_mode(action, window, cx)),
            )
            .on_action(cx.listener(|this, action, window, cx| {
                this.handle_enter_command_mode(action, window, cx)
            }))
            .on_action(cx.listener(|this, action, window, cx| {
                this.select_next(action, SelectionMode::SelectOnly, window, cx)
            }))
            .on_action(cx.listener(|this, action, window, cx| {
                this.select_previous(action, SelectionMode::SelectOnly, window, cx)
            }))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(|this, _: &MoveDown, window, cx| {
                this.select_next(
                    &Default::default(),
                    SelectionMode::SelectAndMove,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &MoveUp, window, cx| {
                this.select_previous(
                    &Default::default(),
                    SelectionMode::SelectAndMove,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &NotebookMoveDown, window, cx| {
                let Some(cell) = this.get_selected_cell() else {
                    return;
                };

                let Some(editor) = cell.editor(cx).cloned() else {
                    return;
                };

                let is_at_last_line = editor.update(cx, |editor, cx| {
                    let display_snapshot = editor.display_snapshot(cx);
                    let selections = editor.selections.all_display(&display_snapshot);
                    if let Some(selection) = selections.last() {
                        let head = selection.head();
                        let cursor_row = head.row();
                        let max_row = display_snapshot.max_point().row();

                        cursor_row >= max_row
                    } else {
                        false
                    }
                });

                if is_at_last_line {
                    this.select_next(
                        &Default::default(),
                        SelectionMode::SelectAndMove,
                        window,
                        cx,
                    );
                } else {
                    editor.update(cx, |editor, cx| {
                        editor.move_down(&Default::default(), window, cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &NotebookMoveUp, window, cx| {
                let Some(cell) = this.get_selected_cell() else {
                    return;
                };

                let Some(editor) = cell.editor(cx).cloned() else {
                    return;
                };

                let is_at_first_line = editor.update(cx, |editor, cx| {
                    let display_snapshot = editor.display_snapshot(cx);
                    let selections = editor.selections.all_display(&display_snapshot);
                    if let Some(selection) = selections.first() {
                        let head = selection.head();
                        let cursor_row = head.row();

                        cursor_row.0 == 0
                    } else {
                        false
                    }
                });

                if is_at_first_line {
                    this.select_previous(
                        &Default::default(),
                        SelectionMode::SelectAndMove,
                        window,
                        cx,
                    );
                } else {
                    editor.update(cx, |editor, cx| {
                        editor.move_up(&Default::default(), window, cx);
                    });
                }
            }))
            .on_action(
                cx.listener(|this, action, window, cx| this.restart_kernel(action, window, cx)),
            )
            .on_action(
                cx.listener(|this, action, window, cx| this.interrupt_kernel(action, window, cx)),
            )
            .when_some(
                self.sync_errors.values().next().cloned(),
                |container, error| container.child(Label::new(error).color(Color::Error)),
            )
            .child(if self.cell_order.is_empty() {
                div()
                    .flex_1()
                    .p_4()
                    .child(Label::new(
                        "This notebook is empty. Use Edit Source to add a cell.",
                    ))
                    .into_any_element()
            } else {
                self.cell_list(window, cx).into_any_element()
            })
            .child(self.render_kernel_status_bar(window, cx))
    }
}

impl Focusable for NotebookEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

pub struct NotebookItem {
    project_path: ProjectPath,
    languages: Arc<LanguageRegistry>,
    pub(super) buffer: Entity<language::Buffer>,
    fields: Vec<CellFields<language::Anchor>>,
    notebook: nbformat::v4::Notebook,
    id: ProjectEntryId,
}

impl project::ProjectItem for NotebookItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<anyhow::Result<Entity<Self>>>> {
        let path = path.clone();
        let project = project.clone();

        if Self::supports_path(project.read(cx), &path, cx) {
            Some(cx.spawn(async move |cx| {
                let buffer = project
                    .update(cx, |project, cx| project.open_buffer(path.clone(), cx))
                    .await?;
                let (source, version) =
                    buffer.read_with(cx, |buffer, _| (buffer.as_rope().clone(), buffer.version()));
                let notebook = cx
                    .background_spawn(
                        async move { super::document::parse_view(&source.to_string()) },
                    )
                    .await?;

                cx.update(|cx| {
                    anyhow::ensure!(
                        buffer.read(cx).version() == version,
                        "The notebook changed while opening"
                    );
                    Self::for_buffer(&project, path, buffer, notebook, cx)
                        .context("Entry not found")
                })
            }))
        } else {
            None
        }
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        Some(self.id)
    }

    fn project_path(&self, _: &App) -> Option<ProjectPath> {
        Some(self.project_path.clone())
    }

    fn is_dirty(&self) -> bool {
        // TODO: Track if notebook metadata or structure has changed
        false
    }
}

impl NotebookItem {
    pub(super) fn supports_path(project: &Project, path: &ProjectPath, cx: &App) -> bool {
        project
            .absolute_path(path, cx)
            .as_ref()
            .and_then(|path| path.extension())
            .is_some_and(|extension| extension.eq_ignore_ascii_case(NOTEBOOK_EXTENSION))
            || path
                .path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case(NOTEBOOK_EXTENSION))
    }

    pub fn language_name(&self) -> Option<String> {
        self.notebook
            .metadata
            .language_info
            .as_ref()
            .map(|l| l.name.clone())
            .or(self
                .notebook
                .metadata
                .kernelspec
                .as_ref()
                .and_then(|spec| spec.language.clone()))
    }

    pub fn notebook_language(&self) -> impl Future<Output = Option<Arc<Language>>> + use<> {
        let language_name = self.language_name().unwrap_or_else(|| "Python".into());
        let languages = self.languages.clone();
        async move { languages.language_for_name(&language_name).await.ok() }
    }

    fn for_buffer(
        project: &Entity<Project>,
        path: ProjectPath,
        buffer: Entity<language::Buffer>,
        parsed: ParsedNotebook,
        cx: &mut App,
    ) -> Option<Entity<Self>> {
        let id = project.read(cx).entry_for_path(&path, cx)?.id;
        let languages = project.read(cx).languages().clone();
        let fields = parsed
            .fields
            .into_iter()
            .map(|fields| {
                fields.map_offsets(|range| {
                    buffer.read(cx).anchor_before(range.start)
                        ..buffer.read(cx).anchor_after(range.end)
                })
            })
            .collect();
        Some(cx.new(|_| Self {
            project_path: path,
            id,
            languages,
            buffer,
            fields,
            notebook: parsed.notebook,
        }))
    }
}

impl KernelSession for NotebookKernelSession {
    fn check_launch(&self, cx: &mut Context<Self>) -> Result<()> {
        let id = cx.entity_id();
        self.0.update(cx, |editor, cx| {
            anyhow::ensure!(
                editor
                    .kernel_session
                    .as_ref()
                    .is_some_and(|session| session.entity_id() == id),
                "Kernel startup was canceled"
            );
            editor.check_execution(cx)
        })?
    }

    fn route(&mut self, message: &JupyterMessage, window: &mut Window, cx: &mut Context<Self>) {
        let id = cx.entity_id();
        self.0
            .update(cx, |editor, cx| {
                if editor
                    .kernel_session
                    .as_ref()
                    .is_none_or(|session| session.entity_id() != id)
                {
                    return;
                }
                if let Err(error) = editor.route_message(message, window, cx) {
                    editor.kernel_notice = Some(error.to_string());
                }
                cx.notify();
            })
            .ok();
    }

    fn kernel_errored(&mut self, error: String, cx: &mut Context<Self>) {
        let id = cx.entity_id();
        self.0
            .update(cx, |editor, cx| {
                if editor
                    .kernel_session
                    .as_ref()
                    .is_some_and(|session| session.entity_id() == id)
                {
                    editor.stop_kernel(cx);
                    editor.kernel_notice = Some(error);
                    cx.notify();
                }
            })
            .ok();
    }
}

impl NotebookEditor {
    fn update_display(
        &mut self,
        update: &jupyter_protocol::UpdateDisplayData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let Some(id) = &update.transient.display_id else {
            return Ok(());
        };
        self.check_execution(cx)?;
        let data = serde_json::to_value(&update.data)?;
        let metadata = serde_json::Value::Object(update.metadata.clone());
        let targets: Vec<_> = self
            .display_slots
            .iter()
            .filter_map(|(cell, slots)| {
                let indices: Vec<_> = slots
                    .iter()
                    .filter(|(display, _)| display == id)
                    .map(|(_, index)| *index)
                    .collect();
                (!indices.is_empty()).then(|| (cell.clone(), indices))
            })
            .collect();
        for (cell_id, indices) in targets {
            let Some(Cell::Code(cell)) = self.cell_map.get(&cell_id).cloned() else {
                continue;
            };
            let replace = |outputs: &mut [serde_json::Value]| -> Result<()> {
                for index in &indices {
                    let output = outputs
                        .get_mut(*index)
                        .context("Displayed output no longer exists")?;
                    output["data"] = data.clone();
                    output["metadata"] = metadata.clone();
                }
                Ok(())
            };
            if let Some(run) = self
                .execution_requests
                .values_mut()
                .find(|run| run.cell_id == cell_id && run.started())
            {
                replace(&mut run.outputs)?;
            } else {
                if cell.read(cx).has_stale_outputs(cx) {
                    continue;
                }
                let index = self
                    .cell_order
                    .iter()
                    .position(|id| *id == cell_id)
                    .context("Displayed cell no longer exists")?;
                let buffer = self.notebook_item.read(cx).buffer.read(cx).snapshot();
                self.patch_cell(index, cx, |fields| {
                    let range = &fields
                        .execution
                        .as_ref()
                        .context("Only code cells have output")?
                        .outputs;
                    let text: String = buffer.text_for_range(range.clone()).collect();
                    let mut outputs: Vec<serde_json::Value> = serde_json::from_str(&text)?;
                    replace(&mut outputs)?;
                    Ok(vec![(range.clone(), serde_json::to_string(&outputs)?)])
                })?;
            }
            cell.update(cx, |cell, cx| {
                cell.update_display(&indices, &update.data, window, cx)
            });
        }
        Ok(())
    }

    fn route_message(
        &mut self,
        message: &JupyterMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        if let JupyterMessageContent::Status(status) = &message.content {
            self.kernel.set_execution_state(&status.execution_state);
        }
        if let JupyterMessageContent::UpdateDisplayData(update) = &message.content {
            return self.update_display(update, window, cx);
        }
        let Some(id) = message.parent_header.as_ref().map(|header| &header.msg_id) else {
            return Ok(());
        };
        let Some(run) = self.execution_requests.get_mut(id) else {
            return Ok(());
        };
        let Some(Cell::Code(cell)) = self.cell_map.get(&run.cell_id).cloned() else {
            return Ok(());
        };
        if !run.started()
            && match &message.content {
                JupyterMessageContent::ExecuteInput(_)
                | JupyterMessageContent::ClearOutput(_)
                | JupyterMessageContent::StreamContent(_)
                | JupyterMessageContent::DisplayData(_)
                | JupyterMessageContent::ExecuteResult(_)
                | JupyterMessageContent::ErrorOutput(_) => true,
                JupyterMessageContent::ExecuteReply(reply) => {
                    reply.status != jupyter_protocol::ReplyStatus::Aborted
                }
                _ => false,
            }
        {
            let previous = cell.update(cx, |cell, cx| {
                let previous = cell.start_execution(run.source.clone());
                cx.notify();
                previous
            });
            run.previous = Some((
                previous,
                self.display_slots.remove(&run.cell_id).unwrap_or_default(),
            ));
        }
        match &message.content {
            JupyterMessageContent::Status(status) => {
                if status.execution_state == jupyter_protocol::ExecutionState::Idle {
                    run.idle = true;
                }
            }
            JupyterMessageContent::ExecuteReply(reply) => {
                run.reply = Some(reply.clone());
            }
            JupyterMessageContent::ClearOutput(clear) => {
                run.clear_on_output = clear.wait;
                if !clear.wait {
                    self.display_slots.remove(&run.cell_id);
                    run.outputs.clear();
                    cell.update(cx, |cell, cx| {
                        cell.clear_outputs();
                        cx.notify();
                    });
                }
            }
            JupyterMessageContent::ExecuteInput(_) => {
                cell.update(cx, |cell, cx| cell.handle_message(message, window, cx))
            }
            JupyterMessageContent::StreamContent(_)
            | JupyterMessageContent::DisplayData(_)
            | JupyterMessageContent::ExecuteResult(_)
            | JupyterMessageContent::ErrorOutput(_) => {
                let mut output = serde_json::to_value(&message.content)?;
                let fields = output.as_object_mut().context("Invalid kernel output")?;
                let transient = fields.remove("transient");
                fields.insert("output_type".into(), message.content.message_type().into());
                let clear = std::mem::take(&mut run.clear_on_output);
                if clear {
                    self.display_slots.remove(&run.cell_id);
                    run.outputs.clear();
                }
                if let Some(id) = transient
                    .as_ref()
                    .and_then(|value| value["display_id"].as_str())
                {
                    self.display_slots
                        .entry(run.cell_id.clone())
                        .or_default()
                        .push((id.to_owned(), run.outputs.len()));
                }
                run.outputs.push(output);
                cell.update(cx, |cell, cx| {
                    if clear {
                        cell.clear_outputs();
                    }
                    cell.handle_message(message, window, cx);
                });
            }
            _ => return Ok(()),
        }
        let Some(reply) = run.reply.as_ref() else {
            return Ok(());
        };
        if !run.idle && reply.status != jupyter_protocol::ReplyStatus::Aborted {
            return Ok(());
        }
        let mut run = self
            .execution_requests
            .remove(id)
            .context("Cell execution not found")?;
        let result = self.commit_execution(&mut run, &cell, window, cx);
        cell.update(cx, |cell, cx| {
            match &result {
                Ok(count) => {
                    cell.set_execution_count(*count);
                    cell.finish_execution();
                }
                Err(_) => {
                    let previous = run.previous.map(|(previous, slots)| {
                        self.display_slots.remove(&run.cell_id);
                        if !slots.is_empty() {
                            self.display_slots.insert(run.cell_id, slots);
                        }
                        previous
                    });
                    cell.cancel_execution(previous);
                }
            }
            cx.notify();
        });
        result.map(|_| ())
    }

    fn commit_execution(
        &mut self,
        run: &mut CellExecution,
        cell: &Entity<CodeCell>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<i32> {
        let reply = run.reply.take().context("Missing kernel reply")?;
        anyhow::ensure!(
            reply.status != jupyter_protocol::ReplyStatus::Aborted,
            "The kernel aborted this cell before it ran. Its previous results are unchanged."
        );
        self.check_execution(cx)?;
        let index = self
            .cell_order
            .iter()
            .position(|id| id == &run.cell_id)
            .context("Executed cell is no longer in the notebook")?;
        let count = i32::try_from(reply.execution_count.0)?;
        if reply.status == jupyter_protocol::ReplyStatus::Error
            && !run
                .outputs
                .iter()
                .any(|output| output["output_type"] == "error")
        {
            let error = reply
                .error
                .context("The kernel reported an execution error without details")?;
            let output = jupyter_protocol::ErrorOutput {
                ename: error.ename,
                evalue: error.evalue,
                traceback: error.traceback,
            };
            run.outputs.push(serde_json::json!({"output_type":"error", "ename":output.ename, "evalue":output.evalue, "traceback":output.traceback}));
            cell.update(cx, |cell, cx| {
                cell.handle_message(&output.into(), window, cx)
            });
        }
        self.persist_execution(index, &run.source, &run.outputs, Some(i64::from(count)), cx)?;
        Ok(count)
    }
}

pub(super) fn upgrade_notebook(
    notebook: nbformat::Notebook,
) -> anyhow::Result<nbformat::v4::Notebook> {
    match notebook {
        nbformat::Notebook::V4(notebook) => Ok(notebook),
        nbformat::Notebook::V4QuirksMode(notebook) => {
            log::warn!("Repairing notebook cell IDs: {:?}", notebook.quirks());
            Ok(notebook.repair())
        }
        nbformat::Notebook::Legacy(notebook) => nbformat::upgrade_legacy_notebook(notebook),
        nbformat::Notebook::V3(notebook) => {
            for worksheet in notebook.worksheets.iter().flatten() {
                for cell in &worksheet.cells {
                    if let nbformat::v3::Cell::Heading { level, .. } = cell {
                        anyhow::ensure!((1..=6).contains(level), "Invalid notebook heading level");
                    }
                }
            }
            nbformat::upgrade_v3_notebook(notebook)
        }
        _ => anyhow::bail!("Unsupported notebook format"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ReplStore;
    use gpui::TestAppContext;
    use project::{FakeFs, Project, ProjectItem as _};
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;
    use util::rel_path::rel_path;

    const NOTEBOOK_WITH_ONE_CODE_CELL: &str = r#"{
        "metadata": {
            "kernelspec": {
                "display_name": "Python 3",
                "language": "python",
                "name": "python3"
            },
            "language_info": {
                "name": "python"
            }
        },
        "nbformat": 4,
        "nbformat_minor": 5,
        "cells": [
            {
                "cell_type": "code",
                "id": "cell-one",
                "metadata": {},
                "execution_count": null,
                "outputs": [],
                "source": ["print('hello')"]
            }
        ]
    }"#;

    #[test]
    fn test_upgrade_notebook_repairs_missing_cell_ids() -> anyhow::Result<()> {
        let mut value: serde_json::Value = serde_json::from_str(NOTEBOOK_WITH_ONE_CODE_CELL)?;
        value["cells"][0]
            .as_object_mut()
            .context("cell is not an object")?
            .remove("id");
        let notebook = nbformat::parse_notebook(&serde_json::to_string(&value)?)?;
        assert!(matches!(notebook, nbformat::Notebook::V4QuirksMode(_)));
        let repaired = upgrade_notebook(notebook)?;
        let serialized = nbformat::serialize_notebook(&nbformat::Notebook::V4(repaired))?;
        assert!(matches!(
            nbformat::parse_notebook(&serialized)?,
            nbformat::Notebook::V4(_)
        ));
        let saved: serde_json::Value = serde_json::from_str(&serialized)?;
        assert_eq!(saved["cells"][0]["source"], value["cells"][0]["source"]);
        assert!(
            saved["cells"][0]["id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        Ok(())
    }

    #[test]
    fn test_upgrade_notebook_preserves_cell_ids() -> anyhow::Result<()> {
        let notebook = upgrade_notebook(nbformat::parse_notebook(NOTEBOOK_WITH_ONE_CODE_CELL)?)?;
        let value = serde_json::to_value(notebook)?;
        assert_eq!(value["cells"][0]["id"], "cell-one");
        Ok(())
    }

    async fn kernel_test_editor<'a>(
        notebook: &str,
        cx: &'a mut TestAppContext,
    ) -> (Entity<NotebookEditor>, &'a mut gpui::VisualTestContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            project::trusted_worktrees::init(Default::default(), cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/notebooks"), json!({ "test.ipynb": notebook }))
            .await;
        let project =
            Project::test_with_worktree_trust(fs, [path!("/notebooks").as_ref()], cx).await;
        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(
                    &project,
                    &ProjectPath {
                        worktree_id,
                        path: rel_path("test.ipynb").into(),
                    },
                    cx,
                )
                .expect("open notebook")
            })
            .await
            .expect("notebook should parse");
        let cx = cx.add_empty_window();
        let editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project.clone(), notebook_item, window, cx))
        });
        (editor, cx)
    }

    fn trust_notebook(editor: &Entity<NotebookEditor>, cx: &mut gpui::VisualTestContext) {
        editor.update(cx, |editor, cx| {
            let store = editor.project.read(cx).worktree_store();
            let worktree_id = editor.worktree_id;
            TrustedWorktrees::try_get_global(cx)
                .unwrap()
                .update(cx, |trust, cx| {
                    trust.trust(
                        &store,
                        [project::trusted_worktrees::PathTrust::Worktree(worktree_id)]
                            .into_iter()
                            .collect(),
                        cx,
                    );
                });
        });
    }

    /// When the configured interpreter doesn't exist (e.g. Python isn't installed),
    /// running a cell must not leave it stuck in the executing state. It should
    /// instead surface the kernel launch error as an error output on the cell.
    #[gpui::test]
    async fn test_run_cell_with_missing_interpreter_shows_error(cx: &mut TestAppContext) {
        let (editor, cx) = kernel_test_editor(NOTEBOOK_WITH_ONE_CODE_CELL, cx).await;
        editor.update_in(cx, |editor, window, cx| {
            assert!(editor.kernel_specification.is_none());
            assert!(editor.kernel_task.is_none());
            editor.run_current_cell(&Run, window, cx);
            assert!(
                editor
                    .kernel_notice
                    .as_deref()
                    .unwrap()
                    .contains("Restricted Mode")
            );
            assert!(editor.kernel_task.is_none());
        });
        trust_notebook(&editor, cx);
        cx.executor().allow_parking();
        let launch = editor.update_in(cx, |editor, window, cx| {
            let spec = LocalKernelSpecification::for_notebook_python(
                path!("/nonexistent/python3").into(),
                "Missing Python".into(),
            )
            .unwrap();
            editor.select_kernel(spec, cx);
            assert!(editor.kernel_task.is_none());
            assert!(matches!(editor.kernel, Kernel::Shutdown));
            editor.run_current_cell(&Run, window, cx);
            editor
                .kernel_task
                .clone()
                .expect("explicit run should start the kernel")
        });
        cx.condition(&editor, |editor, _| editor.kernel_specification.is_none())
            .await;
        let (title, detail) = cx.pending_prompt().expect("setup offer");
        assert_eq!(title, "Set up Python for notebooks?");
        assert!(detail.contains("Could not check Python at"));
        cx.simulate_prompt_answer("Cancel");
        launch.await;
        editor.read_with(cx, |editor, cx| {
            let Some(Cell::Code(cell)) = editor.cell(0) else {
                panic!("expected a code cell")
            };
            let cell = cell.read(cx);
            assert!(!cell.is_executing());
            let [crate::outputs::Output::ErrorOutput(error)] = cell.output_views() else {
                panic!("expected a kernel error")
            };
            assert_eq!(error.ename, "Kernel Error");
            assert!(
                error
                    .traceback
                    .read(cx)
                    .full_text(cx)
                    .contains("Could not check Python at")
            );
            assert_eq!(
                editor.notebook_item.read(cx).buffer.read(cx).text(),
                NOTEBOOK_WITH_ONE_CODE_CELL
            );
        });
    }

    #[gpui::test]
    async fn test_aborted_and_stopped_cells_keep_previous_results(cx: &mut TestAppContext) {
        let saved = json!({"output_type": "stream", "name": "stdout", "text": "saved\n"});
        let notebook = json!({
            "nbformat": 4, "nbformat_minor": 5, "metadata": {}, "cells": [
                {"id":"a", "cell_type":"code", "source":"raise ValueError()", "metadata":{},
                 "execution_count":null, "outputs":[]},
                {"id":"b", "cell_type":"code", "source":"print('partial')", "metadata":{},
                 "execution_count":17, "outputs":[saved]}
            ]
        });
        let (editor, cx) = kernel_test_editor(&notebook.to_string(), cx).await;
        trust_notebook(&editor, cx);
        editor.update_in(cx, |editor, window, cx| {
            let queue = |editor: &mut NotebookEditor, index, cx: &mut Context<NotebookEditor>| {
                let Some(Cell::Code(cell)) = editor.cell(index).cloned() else {
                    panic!("code cell")
                };
                let request: JupyterMessage = ExecuteRequest::default().into();
                let run = CellExecution::queued(&cell, cell.read(cx).current_source(cx), cx);
                editor
                    .execution_requests
                    .insert(request.header.msg_id.clone(), run);
                (request, cell)
            };
            let (a, _) = queue(editor, 0, cx);
            let (b, cell) = queue(editor, 1, cx);
            let assert_visible_result = |text, count, cx: &App| {
                let view = cell.read(cx);
                assert_eq!(view.execution_count(), Some(count));
                let [crate::outputs::Output::Stream { content }] = view.output_views() else {
                    panic!("expected one stream")
                };
                assert_eq!(content.read(cx).full_text(cx), text);
            };
            assert_visible_result("saved\n", 17, cx);
            for message in [
                jupyter_protocol::ErrorOutput {
                    ename: "ValueError".into(),
                    ..Default::default()
                }
                .as_child_of(&a),
                jupyter_protocol::ExecuteReply {
                    status: jupyter_protocol::ReplyStatus::Error,
                    execution_count: 1.into(),
                    ..Default::default()
                }
                .as_child_of(&a),
                jupyter_protocol::Status::idle().as_child_of(&a),
                jupyter_protocol::Status::busy().as_child_of(&b),
                jupyter_protocol::Status::idle().as_child_of(&b),
            ] {
                editor.route_message(&message, window, cx).unwrap();
            }
            let aborted = jupyter_protocol::ExecuteReply {
                status: jupyter_protocol::ReplyStatus::Aborted,
                ..Default::default()
            }
            .as_child_of(&b);
            assert!(
                editor
                    .route_message(&aborted, window, cx)
                    .unwrap_err()
                    .to_string()
                    .contains("aborted")
            );
            assert_visible_result("saved\n", 17, cx);
            assert!(!cell.read(cx).is_executing());
            assert_eq!(cell.read(cx).execution_duration(), None);
            let buffer = editor.notebook_item.read(cx).buffer.clone();
            let after_abort = buffer.read(cx).text();
            let doc: serde_json::Value = serde_json::from_str(&after_abort).unwrap();
            assert_eq!(doc["cells"][0]["outputs"][0]["ename"], "ValueError");
            assert_eq!(doc["cells"][1], notebook["cells"][1]);

            let (rerun, _) = queue(editor, 1, cx);
            for message in [
                jupyter_protocol::ExecuteInput {
                    execution_count: 2.into(),
                    ..Default::default()
                }
                .as_child_of(&rerun),
                jupyter_protocol::StreamContent::stdout("partial\n").as_child_of(&rerun),
            ] {
                editor.route_message(&message, window, cx).unwrap();
            }
            assert_visible_result("partial\n", 2, cx);
            editor.stop_kernel(cx);
            assert_visible_result("saved\n", 17, cx);
            assert!(
                editor
                    .kernel_notice
                    .as_deref()
                    .unwrap()
                    .contains("previous results are unchanged")
            );
            assert!(!cell.read(cx).is_executing());
            assert_eq!(cell.read(cx).execution_duration(), None);
            assert_eq!(buffer.read(cx).text(), after_abort);
        });
    }

    #[gpui::test]
    async fn test_notebook_run_persists_matching_outputs_after_reply_and_idle(
        cx: &mut TestAppContext,
    ) {
        let (editor, cx) = kernel_test_editor(NOTEBOOK_WITH_ONE_CODE_CELL, cx).await;
        trust_notebook(&editor, cx);
        editor.update_in(cx, |editor, window, cx| {
            let request: JupyterMessage = ExecuteRequest::default().into();
            let other: JupyterMessage = ExecuteRequest::default().into();
            let id = editor.cell_order.first().unwrap().clone();
            let source = editor.cell(0).unwrap().current_source(cx);
            editor.execution_requests.insert(
                request.header.msg_id.clone(),
                CellExecution::new(id, source),
            );
            let duplicate = CellRun {
                cell_id: editor.cell_order[0].clone(),
                source: editor.cell(0).unwrap().current_source(cx),
                advance: true,
            };
            let error = editor
                .dispatch_cell(&duplicate, window, cx)
                .expect_err("duplicate run");
            assert!(error.to_string().contains("already running"));
            let buffer = editor.notebook_item.read(cx).buffer.clone();
            let stream = jupyter_protocol::StreamContent::stdout("from this request\n");
            let reply = jupyter_protocol::ExecuteReply {
                execution_count: 9.into(),
                ..Default::default()
            };
            for msg in [
                stream.as_child_of(&other),
                jupyter_protocol::Status::idle().as_child_of(&other),
                reply.as_child_of(&request),
                jupyter_protocol::StreamContent::stdout("cleared").as_child_of(&request),
                jupyter_protocol::ClearOutput { wait: true }.as_child_of(&request),
                stream.as_child_of(&request),
            ] {
                editor
                    .route_message(&msg, window, cx)
                    .expect("route kernel message");
            }
            assert_eq!(buffer.read(cx).text(), NOTEBOOK_WITH_ONE_CODE_CELL);
            editor
                .route_message(
                    &jupyter_protocol::Status::idle().as_child_of(&request),
                    window,
                    cx,
                )
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(&buffer.read(cx).text()).unwrap();
            assert_eq!(value["cells"][0]["execution_count"], 9);
            assert_eq!(
                value["cells"][0]["outputs"],
                json!([{"output_type":"stream","name":"stdout","text":"from this request\n"}])
            );
        });
    }

    #[gpui::test]
    async fn test_display_updates_replace_matching_slots_across_cells(cx: &mut TestAppContext) {
        use jupyter_protocol::{
            DisplayData, ExecuteReply, MediaType, Status, StreamContent, Transient,
            UpdateDisplayData,
        };
        let mut fixture: serde_json::Value =
            serde_json::from_str(NOTEBOOK_WITH_ONE_CODE_CELL).unwrap();
        let mut next = fixture["cells"][0].clone();
        next["id"] = json!("second");
        fixture["cells"].as_array_mut().unwrap().push(next);
        let (editor, cx) = kernel_test_editor(&fixture.to_string(), cx).await;
        trust_notebook(&editor, cx);
        editor.update_in(cx, |editor, window, cx| {
            for index in 0..2 {
                let request: JupyterMessage = ExecuteRequest::default().into();
                let cell = editor.cell(index).unwrap();
                editor.execution_requests.insert(request.header.msg_id.clone(), CellExecution::new(cell.id(cx), cell.current_source(cx)));
                let original = DisplayData {
                    data: vec![MediaType::Plain("initial".into())].into(),
                    transient: Some(Transient { display_id: Some("figure".into()) }),
                    ..Default::default()
                };
                let final_text = if index == 0 { "first update" } else { "final update" };
                let mut update = UpdateDisplayData::new(vec![MediaType::Plain(final_text.into())].into(), "figure");
                update.metadata.insert("phase".into(), json!(index));
                for msg in [
                    original.as_child_of(&request),
                    StreamContent::stdout("untouched").as_child_of(&request),
                    original.as_child_of(&request),
                    update.as_child_of(&request),
                    ExecuteReply { execution_count: (index + 1).into(), ..Default::default() }.as_child_of(&request),
                    Status::idle().as_child_of(&request),
                ] {
                    editor.route_message(&msg, window, cx).unwrap();
                }
                let text = editor.notebook_item.read(cx).buffer.read(cx).text();
                let document: serde_json::Value = serde_json::from_str(&text).unwrap();
                for target in 0..=index {
                    let cell = &document["cells"][target];
                    assert_eq!(cell["execution_count"], target + 1);
                    assert_eq!(cell["outputs"][1]["text"], "untouched");
                    let Some(Cell::Code(view)) = editor.cell(target) else { panic!("expected code cell") };
                    for slot in [0, 2] {
                        assert_eq!(cell["outputs"][slot], json!({"output_type": "display_data", "data": {"text/plain": final_text}, "metadata": {"phase": index}}));
                        let crate::outputs::Output::Plain { content, .. } = &view.read(cx).output_views()[slot] else { panic!("expected plain output") };
                        assert_eq!(content.read(cx).full_text(cx).trim_end(), final_text);
                    }
                }
            }
        });
        let first = editor.read_with(cx, |editor, cx| {
            editor.cell(0).unwrap().editor(cx).unwrap().clone()
        });
        first.update_in(cx, |editor, window, cx| {
            editor.set_text("changed since execution", window, cx)
        });
        cx.run_until_parked();
        editor.update_in(cx, |editor, window, cx| {
            let later =
                UpdateDisplayData::new(vec![MediaType::Plain("later".into())].into(), "figure");
            editor.route_message(&later.into(), window, cx).unwrap();
            let text = editor.notebook_item.read(cx).buffer.read(cx).text();
            let document: serde_json::Value = serde_json::from_str(&text).unwrap();
            for (index, expected) in ["final update", "later"].into_iter().enumerate() {
                let saved = &document["cells"][index];
                assert_eq!(saved["execution_count"], index + 1);
                let Some(Cell::Code(view)) = editor.cell(index) else {
                    panic!("expected code cell")
                };
                for slot in [0, 2] {
                    assert_eq!(saved["outputs"][slot]["data"]["text/plain"], expected);
                    let crate::outputs::Output::Plain { content, .. } =
                        &view.read(cx).output_views()[slot]
                    else {
                        panic!("expected plain output")
                    };
                    assert_eq!(content.read(cx).full_text(cx).trim_end(), expected);
                }
            }
            editor.stop_kernel(cx);
            let before = editor.notebook_item.read(cx).buffer.read(cx).text();
            let late = UpdateDisplayData::new(
                vec![MediaType::Plain("old kernel".into())].into(),
                "figure",
            );
            editor.route_message(&late.into(), window, cx).unwrap();
            assert_eq!(editor.notebook_item.read(cx).buffer.read(cx).text(), before);
        });
    }

    #[gpui::test]
    async fn test_save_and_reopen_a_kernel_chart(cx: &mut TestAppContext) {
        use crate::notebook::{
            NotebookPreview,
            preview::preview_tests::{notebook_workspace, open_file},
        };
        use project::Fs as _;

        const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6fptVAAAACklEQVR4nGP4DwABAQEAsTj2FAAAAABJRU5ErkJggg==";
        let (fs, workspace, cx) =
            notebook_workspace(json!({"chart.ipynb": NOTEBOOK_WITH_ONE_CODE_CELL}), cx).await;
        open_file("chart.ipynb", &workspace, cx).await;
        let active_notebook = |cx: &mut gpui::VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .active_item_as::<NotebookPreview>(cx)
                    .expect("notebook tab")
                    .read(cx)
                    .notebook
                    .clone()
                    .expect("cell editor")
            })
        };
        let notebook = active_notebook(cx);
        trust_notebook(&notebook, cx);
        let data =
            json!({"image/png": PNG, "text/plain": "Figure", "text/html": "<span>Figure</span>"});
        notebook.update_in(cx, |editor, window, cx| {
            let request: JupyterMessage = ExecuteRequest::default().into();
            let cell = editor.cell(0).expect("code cell");
            editor.execution_requests.insert(
                request.header.msg_id.clone(),
                CellExecution::new(cell.id(cx), cell.current_source(cx)),
            );
            let display = jupyter_protocol::DisplayData::new(
                serde_json::from_value(data.clone()).expect("valid MIME bundle"),
            );
            let reply = jupyter_protocol::ExecuteReply {
                execution_count: 3.into(),
                ..Default::default()
            };
            for msg in [
                display.as_child_of(&request),
                jupyter_protocol::Status::idle().as_child_of(&request),
                reply.as_child_of(&request),
            ] {
                editor
                    .route_message(&msg, window, cx)
                    .expect("route kernel result");
            }
        });
        drop(notebook);
        cx.dispatch_action(workspace::Save { save_intent: None });
        cx.run_until_parked();
        let bytes = fs
            .load_bytes(std::path::Path::new(path!("/notebooks/chart.ipynb")))
            .await
            .expect("saved notebook");
        let saved: serde_json::Value = serde_json::from_slice(&bytes).expect("valid notebook");
        assert_eq!(saved["cells"][0]["execution_count"], 3);
        assert_eq!(saved["cells"][0]["outputs"][0]["data"], data);
        cx.dispatch_action(workspace::pane::CloseActiveItem::default());
        cx.run_until_parked();
        open_file("chart.ipynb", &workspace, cx).await;
        active_notebook(cx).read_with(cx, |editor, cx| {
            assert!(matches!(editor.kernel, Kernel::Shutdown));
            let Some(Cell::Code(cell)) = editor.cell(0) else {
                panic!("expected a code cell")
            };
            assert!(matches!(
                cell.read(cx).output_views(),
                [crate::outputs::Output::Image { .. }]
            ));
        });
    }

    #[gpui::test]
    async fn test_restricting_worktree_discards_in_flight_cell_output(cx: &mut TestAppContext) {
        let (editor, cx) = kernel_test_editor(NOTEBOOK_WITH_ONE_CODE_CELL, cx).await;
        trust_notebook(&editor, cx);
        let request: JupyterMessage = ExecuteRequest::default().into();
        editor.update_in(cx, |editor, window, cx| {
            let cell = editor.cell(0).unwrap();
            let run = CellExecution::new(cell.id(cx), cell.current_source(cx));
            editor
                .execution_requests
                .insert(request.header.msg_id.clone(), run);
            editor
                .route_message(
                    &jupyter_protocol::StreamContent::stdout("in flight").as_child_of(&request),
                    window,
                    cx,
                )
                .unwrap();
            let store = editor.project.read(cx).worktree_store().downgrade();
            let id = project::trusted_worktrees::PathTrust::Worktree(editor.worktree_id);
            TrustedWorktrees::try_get_global(cx)
                .unwrap()
                .update(cx, |trust, cx| {
                    trust.restrict(store, [id].into_iter().collect(), cx)
                });
        });
        cx.run_until_parked();
        editor.update_in(cx, |editor, window, cx| {
            for message in [
                jupyter_protocol::Status::idle().as_child_of(&request),
                jupyter_protocol::ExecuteReply::default().as_child_of(&request),
            ] {
                editor.route_message(&message, window, cx).unwrap();
            }
            assert!(
                editor
                    .kernel_notice
                    .as_deref()
                    .unwrap()
                    .contains("Restricted Mode")
            );
            assert_eq!(
                editor.notebook_item.read(cx).buffer.read(cx).text(),
                NOTEBOOK_WITH_ONE_CODE_CELL
            );
        });
    }

    #[gpui::test]
    async fn test_run_and_advance_during_kernel_setup_stays_on_cell(cx: &mut TestAppContext) {
        let mut value: serde_json::Value =
            serde_json::from_str(NOTEBOOK_WITH_ONE_CODE_CELL).unwrap();
        let mut second = value["cells"][0].clone();
        second["id"] = "cell-two".into();
        value["cells"].as_array_mut().unwrap().push(second);
        let (editor, cx) = kernel_test_editor(&value.to_string(), cx).await;
        trust_notebook(&editor, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.kernel_task = Some(Task::ready(()).shared());
            editor.run_and_advance(&RunAndAdvance, window, cx);
            assert_eq!(editor.selected_cell_index, 0);
            assert!(
                editor
                    .kernel_notice
                    .as_deref()
                    .unwrap()
                    .contains("not queued")
            );
        });
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn test_selected_python_without_ipykernel_offers_setup_without_advancing(
        cx: &mut TestAppContext,
    ) {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let interpreter = root.path().join("python");
        std::fs::write(&interpreter, "#!/bin/sh\nexit 77\n").unwrap();
        std::fs::set_permissions(&interpreter, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_str(NOTEBOOK_WITH_ONE_CODE_CELL).unwrap();
        let mut second = value["cells"][0].clone();
        second["id"] = "second".into();
        value["cells"].as_array_mut().unwrap().push(second);
        let raw = value.to_string();
        let (editor, cx) = kernel_test_editor(&raw, cx).await;
        trust_notebook(&editor, cx);
        cx.executor().allow_parking();
        let run = editor.update_in(cx, |editor, window, cx| {
            let spec =
                LocalKernelSpecification::for_notebook_python(interpreter, "System Python".into())
                    .unwrap();
            editor.select_kernel(spec, cx);
            editor.run_and_advance(&RunAndAdvance, window, cx);
            assert_eq!(editor.selected_cell_index, 0, "setup is not execution");
            editor.kernel_task.clone().unwrap()
        });
        cx.condition(&editor, |editor, _| editor.kernel_specification.is_none())
            .await;
        let (_, detail) = cx.pending_prompt().expect("consent before installing");
        assert!(detail.contains("ipykernel is not installed in System Python"));
        cx.simulate_prompt_answer("Cancel");
        run.await;
        editor.read_with(cx, |editor, cx| {
            assert_eq!(editor.selected_cell_index, 0);
            assert!(editor.kernel_session.is_none());
            assert!(
                editor.kernel_specification.is_none(),
                "the next Run can rediscover a usable kernel"
            );
            assert_eq!(editor.notebook_item.read(cx).buffer.read(cx).text(), raw);
        });
    }

    /// Opening a notebook as a single file (its own worktree) leaves the
    /// worktree-relative path empty, so only the absolute path carries the
    /// `.ipynb` extension. `try_open` must still recognize it as a notebook.
    #[gpui::test]
    async fn test_open_single_file_notebook(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/notebooks"),
            json!({ "single.ipynb": NOTEBOOK_WITH_ONE_CODE_CELL }),
        )
        .await;

        let project =
            Project::test(fs.clone(), [path!("/notebooks/single.ipynb").as_ref()], cx).await;
        cx.update(|cx| ReplStore::init(fs.clone(), cx));

        let project_path = project.read_with(cx, |project, cx| {
            let worktree = project.worktrees(cx).next().unwrap();
            let worktree = worktree.read(cx);
            assert!(
                worktree.is_single_file(),
                "opening a bare .ipynb should create a single-file worktree"
            );
            ProjectPath {
                worktree_id: worktree.id(),
                path: worktree.root_entry().unwrap().path.clone(),
            }
        });

        assert!(
            project_path.path.extension().is_none(),
            "single-file worktree relative path should have no extension"
        );

        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(&project, &project_path, cx)
                    .expect("single-file .ipynb should open as a notebook")
            })
            .await
            .expect("notebook should parse");

        notebook_item.read_with(cx, |item, _| {
            assert_eq!(item.notebook.cells.len(), 1);
        });
        let cx = cx.add_empty_window();
        let editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project, notebook_item, window, cx))
        });
        let interpreter = editor
            .read_with(cx, |editor, cx| editor.project_python(true, cx).unwrap())
            .await;
        assert!(
            interpreter.is_none(),
            "opening a single file does not trust Python environments in its parent directory"
        );
    }

    /// Notebooks must be saved through the project rather than through the
    /// client's own filesystem, otherwise a remote notebook's path is resolved
    /// against the local machine and the save fails.
    #[gpui::test]
    async fn test_save_goes_through_the_project(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/notebooks"),
            json!({ "test.ipynb": NOTEBOOK_WITH_ONE_CODE_CELL }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/notebooks").as_ref()], cx).await;
        cx.update(|cx| ReplStore::init(fs.clone(), cx));

        let project_path = project.read_with(cx, |project, cx| ProjectPath {
            worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
            path: rel_path("test.ipynb").into(),
        });

        let notebook_item = cx
            .update(|cx| {
                NotebookItem::try_open(&project, &project_path, cx)
                    .expect("ipynb files should be openable as notebooks")
            })
            .await
            .expect("notebook should parse");

        // Held across the save: a save that bypasses the project writes the file
        // behind this buffer's back, leaving it stale.
        let buffer = project
            .update(cx, |project, cx| {
                project.open_buffer(project_path.clone(), cx)
            })
            .await
            .expect("notebook buffer should open");

        // Rendering the notebook animates the kernel status icon, which makes
        // `run_until_parked` spin forever; only the editor entity is needed here.
        let cx = cx.add_empty_window();
        let notebook_editor = cx.update(|window, cx| {
            cx.new(|cx| NotebookEditor::new(project.clone(), notebook_item, window, cx))
        });

        let cell_editor = notebook_editor.read_with(cx, |notebook_editor, cx| {
            let cell_id = notebook_editor
                .cell_order
                .first()
                .expect("notebook has one cell");
            let Some(Cell::Code(cell)) = notebook_editor.cell_map.get(cell_id) else {
                panic!("expected a code cell");
            };
            cell.read(cx).editor().clone()
        });
        cell_editor.update_in(cx, |cell_editor, window, cx| {
            cell_editor.set_text("print('goodbye')", window, cx);
        });

        notebook_editor
            .update_in(cx, |notebook_editor, window, cx| {
                notebook_editor.save(SaveOptions::default(), project.clone(), window, cx)
            })
            .await
            .expect("saving the notebook should succeed");

        let saved = String::from_utf8(
            fs.read_file_sync(path!("/notebooks/test.ipynb"))
                .expect("notebook should still exist"),
        )
        .expect("notebook should be valid UTF-8");
        assert!(
            saved.contains("print('goodbye')"),
            "the edited cell should be written to the notebook, got: {saved}"
        );

        buffer.read_with(cx, |buffer, _| {
            assert_eq!(
                buffer.text(),
                saved,
                "the project's buffer should hold the saved notebook"
            );
            assert!(!buffer.is_dirty(), "saving should leave the buffer clean");
        });
    }
}
