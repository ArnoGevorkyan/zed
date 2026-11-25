use std::path::Path;

use anyhow::{Context as _, Result};
use collections::HashSet;
use editor::{Editor, EditorEvent};
use feature_flags::{FeatureFlagAppExt as _, NotebookFeatureFlag};
use gpui::{
    App, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Global, ListAlignment, ListState,
    Task, WeakEntity, list,
};
use markdown::MarkdownLabel;
use project::{Project, ProjectPath};
use ui::prelude::*;
use workspace::item::{ItemBufferKind, SerializableItem, TabContentParams};
use workspace::{Item, ItemHandle, ItemId, Pane, Workspace, WorkspaceId};
use zed_actions::notebook::{EditSource, OpenNotebook};

use super::notebook_ui::upgrade_notebook;

#[cfg(test)]
#[path = "preview_tests.rs"]
mod preview_tests;

#[derive(Default)]
struct SourceEditors(HashSet<EntityId>);
impl Global for SourceEditors {}

enum PreviewCell {
    Markdown(SharedString),
    Code(SharedString),
    Raw(SharedString),
}

impl PreviewCell {
    fn parse(source: &str) -> Result<Vec<Self>> {
        let notebook = upgrade_notebook(nbformat::parse_notebook(source)?)?;
        Ok(notebook
            .cells
            .into_iter()
            .map(|cell| match cell {
                nbformat::v4::Cell::Markdown { source, .. } => {
                    Self::Markdown(source.concat().into())
                }
                nbformat::v4::Cell::Code { source, .. } => Self::Code(source.concat().into()),
                nbformat::v4::Cell::Raw { source, .. } => Self::Raw(source.concat().into()),
            })
            .collect())
    }
}

pub struct NotebookPreview {
    workspace: WeakEntity<Workspace>,
    editor: Entity<Editor>,
    focus_handle: FocusHandle,
    cells: Vec<PreviewCell>,
    list: ListState,
    alongside_source: bool,
    restore_source: bool,
}

pub(super) fn init(cx: &mut App) {
    cx.default_global::<SourceEditors>();
    workspace::register_serializable_item::<NotebookPreview>(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else { return };
        cx.subscribe_in(&cx.entity(), window, |workspace, _, event, window, cx| {
            let workspace::Event::ItemAdded { item } = event else {
                return;
            };
            let Some(editor) = item.downcast::<Editor>() else {
                return;
            };
            if workspace.is_restoring()
                || cx.has_flag::<NotebookFeatureFlag>()
                || std::env::var_os("LOCAL_NOTEBOOK_DEV").is_some()
                || cx.global::<SourceEditors>().0.contains(&item.item_id())
                || item.is_dirty(cx)
            {
                return;
            }
            NotebookPreview::open(workspace, editor, false, window, cx);
        })
        .detach();
        workspace.register_action(|workspace, _: &OpenNotebook, window, cx| {
            if let Some(editor) = workspace
                .item_for_action(window, cx)
                .and_then(|item| item.downcast::<Editor>())
            {
                NotebookPreview::open(workspace, editor, true, window, cx);
            }
        });
    })
    .detach();
}

impl NotebookPreview {
    fn open(
        workspace: &mut Workspace,
        editor: Entity<Editor>,
        explicit: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else {
            return;
        };
        let is_notebook = |buffer: &language::Buffer, cx: &App| {
            buffer.file().is_some_and(|file| {
                Path::new(file.file_name(cx))
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("ipynb"))
            })
        };
        if !is_notebook(buffer.read(cx), cx) {
            return;
        }
        let Some(pane) = workspace.pane_for_item_id(editor.entity_id()) else {
            return;
        };
        let version = buffer.read(cx).version();
        let text = buffer.read(cx).as_rope().clone();
        let parse = cx.background_spawn(async move { PreviewCell::parse(&text.to_string()) });
        cx.spawn_in(window, async move |workspace, cx| {
            let cells = parse.await;
            workspace.update_in(cx, |workspace, window, cx| {
                let dirty = editor.is_dirty(cx);
                if workspace.pane_for_item_id(editor.entity_id()).as_ref() != Some(&pane)
                    || pane.read(cx).index_for_item(&editor).is_none()
                    || buffer.read(cx).version() != version
                    || !is_notebook(buffer.read(cx), cx)
                    || (!explicit
                        && (dirty
                            || workspace.is_restoring()
                            || cx.global::<SourceEditors>().0.contains(&editor.entity_id())))
                {
                    return;
                }
                let existing = pane.read(cx).items_of_type::<Self>().find(|preview| {
                    preview
                        .read(cx)
                        .editor
                        .read(cx)
                        .buffer()
                        .read(cx)
                        .as_singleton()
                        .as_ref()
                        == Some(&buffer)
                });
                if let Some(existing) = existing {
                    if explicit {
                        workspace.activate_item(&existing, true, true, window, cx);
                    }
                    return;
                }
                let cells = match cells {
                    Ok(cells) => cells,
                    Err(error) => {
                        if explicit {
                            workspace.show_error(error.context("Could not preview notebook"), cx);
                        }
                        return;
                    }
                };
                let owner = cx.weak_entity();
                let preview = cx
                    .new(|cx| Self::new(owner, editor.clone(), Some(cells), explicit, window, cx));
                pane.update(cx, |pane, cx| {
                    if explicit {
                        pane.add_item(Box::new(preview), true, true, None, window, cx);
                    } else {
                        Self::replace_tab(pane, &editor, Box::new(preview), false, window, cx);
                    }
                });
            })
        })
        .detach_and_log_err(cx);
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        editor: Entity<Editor>,
        cells: Option<Vec<PreviewCell>>,
        alongside_source: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe_in(&editor, window, |this, _, event, window, cx| {
            if matches!(
                event,
                EditorEvent::BufferEdited | EditorEvent::FileHandleChanged
            ) {
                this.restore_source = true;
                this.show_source(false, window, cx);
            }
        })
        .detach();
        let restore_source = cells.is_none();
        let cells = cells.unwrap_or_default();
        Self {
            workspace,
            editor,
            focus_handle: cx.focus_handle(),
            list: ListState::new(cells.len(), ListAlignment::Top, px(400.)),
            cells,
            alongside_source,
            restore_source,
        }
    }

    fn replace_tab(
        pane: &mut Pane,
        from: &dyn ItemHandle,
        to: Box<dyn ItemHandle>,
        explicit: bool,
        window: &mut Window,
        cx: &mut Context<Pane>,
    ) {
        let Some(index) = pane.index_for_item(from) else {
            return;
        };
        let previous_active = pane.active_item();
        let was_active = previous_active
            .as_ref()
            .is_some_and(|item| item.item_id() == from.item_id());
        let focus = explicit || from.item_focus_handle(cx).contains_focused(window, cx);
        let temporary = pane.is_active_preview_item(from.item_id());
        let pinned_count = pane.pinned_count();
        let to_id = to.item_id();
        pane.remove_item(from.item_id(), false, false, window, cx);
        pane.add_item_inner(
            to,
            focus,
            focus,
            explicit || was_active,
            Some(index),
            window,
            cx,
        );
        if index < pinned_count {
            pane.set_pinned_count(pinned_count);
        }
        if temporary {
            pane.replace_preview_item_id(to_id, window, cx);
        }
        if !explicit
            && !was_active
            && let Some(previous) = previous_active
            && let Some(index) = pane.index_for_item(previous.as_ref())
        {
            pane.activate_item(index, false, false, window, cx);
        }
    }

    fn show_source(&self, explicit: bool, window: &mut Window, cx: &mut Context<Self>) {
        let preview = cx.entity();
        let editor = self.editor.clone();
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            workspace.update(cx, |workspace, cx| {
                let Some(pane) = workspace.pane_for_item_id(preview.entity_id()) else {
                    return;
                };
                let source_buffer = editor.read(cx).buffer().read(cx).as_singleton();
                let editor = pane
                    .read(cx)
                    .items_of_type::<Editor>()
                    .find(|item| item.read(cx).buffer().read(cx).as_singleton() == source_buffer)
                    .unwrap_or(editor);
                let source_id = editor.entity_id();
                if cx.global_mut::<SourceEditors>().0.insert(source_id) {
                    App::observe_release(cx, &editor, move |_, cx| {
                        cx.global_mut::<SourceEditors>().0.remove(&source_id);
                    })
                    .detach();
                }
                if workspace.pane_for_item_id(source_id).is_some() {
                    if explicit {
                        workspace.activate_item(&editor, true, true, window, cx);
                    }
                    pane.update(cx, |pane, cx| {
                        pane.remove_item(preview.entity_id(), false, false, window, cx)
                    });
                } else {
                    pane.update(cx, |pane, cx| {
                        Self::replace_tab(pane, &preview, Box::new(editor), explicit, window, cx)
                    });
                }
            });
        });
    }
}

impl EventEmitter<()> for NotebookPreview {}

impl Focusable for NotebookPreview {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for NotebookPreview {
    type Event = ();

    fn tab_content_text(&self, detail: usize, cx: &App) -> SharedString {
        let title = self.editor.read(cx).tab_content_text(detail, cx);
        if self.alongside_source {
            format!("Preview {title}").into()
        } else {
            title
        }
    }

    fn tab_content(&self, params: TabContentParams, _: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or(0), cx))
            .single_line()
            .color(params.text_color())
            .when(params.preview, |label| label.italic())
            .into_any_element()
    }

    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Book))
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }

    fn buffer_kind(&self, _: &App) -> ItemBufferKind {
        if self.alongside_source {
            ItemBufferKind::None
        } else {
            ItemBufferKind::Singleton
        }
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        visit: &mut dyn FnMut(EntityId, &dyn project::ProjectItem),
    ) {
        if !self.alongside_source {
            self.editor.read(cx).for_each_project_item(cx, visit);
        }
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn capability(&self, _: &App) -> language::Capability {
        language::Capability::ReadOnly
    }

    fn added_to_workspace(
        &mut self,
        _: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.restore_source {
            self.show_source(false, window, cx);
        }
    }
}

impl Render for NotebookPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().editor_background)
            .on_action(
                cx.listener(|this, _: &EditSource, window, cx| this.show_source(true, window, cx)),
            )
            .child(
                h_flex()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(Label::new("Notebook Preview").color(Color::Muted))
                    .child(
                        Button::new("edit-source", "Edit Source")
                            .start_icon(Icon::new(IconName::Code))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_source(true, window, cx)
                            })),
                    ),
            )
            .when_else(
                self.cells.is_empty(),
                |view| {
                    view.child(
                        div()
                            .p_4()
                            .child(Label::new("This notebook is empty.").color(Color::Muted)),
                    )
                },
                |container| {
                    container.child(
                        list(self.list.clone(), move |index, _, cx| {
                            let cell = view.read(cx).cells.get(index);
                            let content = match cell {
                                Some(PreviewCell::Markdown(text)) => {
                                    MarkdownLabel(text.clone()).into_any_element()
                                }
                                Some(PreviewCell::Code(text) | PreviewCell::Raw(text)) => div()
                                    .w_full()
                                    .p_3()
                                    .rounded_md()
                                    .bg(cx.theme().colors().editor_subheader_background)
                                    .font_buffer(cx)
                                    .child(text.clone())
                                    .into_any_element(),
                                None => return div().into_any_element(),
                            };
                            div()
                                .id(("notebook-cell", index))
                                .w_full()
                                .px_4()
                                .py_2()
                                .child(content)
                                .into_any_element()
                        })
                        .flex_1()
                        .w_full(),
                    )
                },
            )
    }
}

impl SerializableItem for NotebookPreview {
    fn serialized_item_kind() -> &'static str {
        "NotebookPreview"
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let path = self.editor.read(cx).active_project_path(cx)?;
        let path = workspace.project().read(cx).absolute_path(&path, cx)?;
        let db = persistence::NotebookPreviewDb::global(cx);
        let alongside_source = self.alongside_source;
        Some(cx.background_spawn(async move {
            db.save_path(item_id, workspace_id, path, alongside_source)
                .await
        }))
    }

    fn should_serialize(&self, _: &Self::Event) -> bool {
        false
    }

    fn deserialize(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = persistence::NotebookPreviewDb::global(cx);
        window.spawn(cx, async move |cx| {
            let (path, alongside_source) = db
                .get_path(item_id, workspace_id)?
                .context("No saved notebook path")?;
            let (worktree, path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(path, false, cx)
                })
                .await?;
            let path = ProjectPath {
                worktree_id: worktree.read_with(cx, |worktree, _| worktree.id()),
                path,
            };
            let buffer = project
                .update(cx, |project, cx| project.open_buffer(path, cx))
                .await?;
            let (text, version) =
                buffer.read_with(cx, |buffer, _| (buffer.as_rope().clone(), buffer.version()));
            let cells = cx
                .background_spawn(async move { PreviewCell::parse(&text.to_string()).ok() })
                .await;
            cx.update(|window, cx| {
                let cells = cells.filter(|_| {
                    (alongside_source || !buffer.read(cx).is_dirty())
                        && buffer.read(cx).version() == version
                });
                let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx));
                cx.new(|cx| Self::new(workspace, editor, cells, alongside_source, window, cx))
            })
        })
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        workspace::delete_unloaded_items(
            alive_items,
            workspace_id,
            "notebook_previews",
            &persistence::NotebookPreviewDb::global(cx),
            cx,
        )
    }
}

mod persistence {
    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use std::path::PathBuf;
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub(super) struct NotebookPreviewDb(ThreadSafeConnection);

    impl Domain for NotebookPreviewDb {
        const NAME: &str = stringify!(NotebookPreviewDb);
        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE notebook_previews (
                workspace_id INTEGER,
                item_id INTEGER,
                path BLOB NOT NULL,
                alongside_source INTEGER NOT NULL,
                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id) ON DELETE CASCADE
            ) STRICT;
        )];
    }

    db::static_connection!(NotebookPreviewDb, [WorkspaceDb]);

    impl NotebookPreviewDb {
        query! {
            pub async fn save_path(item_id: ItemId, workspace_id: WorkspaceId, path: PathBuf, alongside_source: bool) -> Result<()> {
                INSERT OR REPLACE INTO notebook_previews(item_id, workspace_id, path, alongside_source) VALUES (?, ?, ?, ?)
            }
        }
        query! {
            pub fn get_path(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<(PathBuf, bool)>> {
                SELECT path, alongside_source FROM notebook_previews WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}
