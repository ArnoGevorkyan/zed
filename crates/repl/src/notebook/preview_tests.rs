use super::NotebookPreview;
use editor::Editor;
use feature_flags::{
    FeatureFlag as _, FeatureFlagAppExt as _, FeatureFlagStore, NotebookFeatureFlag,
};
use gpui::{Entity, TestAppContext, UpdateGlobal as _, VisualTestContext};
use project::trusted_worktrees::TrustedWorktrees;
use project::{FakeFs, Fs as _, Project};
use serde_json::{Value, json};
use settings::SettingsStore;
use std::path::Path;
use std::sync::Arc;
use util::path;
use util::rel_path::rel_path;
use workspace::item::ItemHandle;
use workspace::{AppState, MultiWorkspace, Workspace};
use zed_actions::notebook::{EditSource, OpenNotebook};

const NOTEBOOK: &str = r##"{
  "nbformat_minor": 5,  "nbformat": 4,
  "metadata": { "custom": { "keep": true } },
  "cells": [
    { "cell_type": "markdown", "id": "intro", "metadata": {"tags":["note"]},
      "source": ["# Field notes\n", "![sample](attachment:sample.png)"],
      "attachments": {"sample.png":{"image/png":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6fptVAAAACklEQVR4nGP4DwABAQEAsTj2FAAAAABJRU5ErkJggg=="}} },
    { "cell_type": "code", "id": "compute", "execution_count": 7,
      "metadata": {}, "source": ["print('from source')"],
      "outputs": [{"output_type":"stream", "name":"stdout", "text":["saved result\n"]}] }
  ]
}
"##;

pub(in crate::notebook) async fn notebook_workspace(
    files: Value,
    cx: &mut TestAppContext,
) -> (Arc<FakeFs>, Entity<Workspace>, &mut VisualTestContext) {
    let fs = cx.update(|cx| {
        let state = AppState::test(cx);
        editor::init(cx);
        FeatureFlagStore::init(cx);
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings
                    .feature_flags
                    .get_or_insert_default()
                    .insert(NotebookFeatureFlag::NAME.into(), "off".into());
            });
        });
        assert!(!cx.has_flag::<NotebookFeatureFlag>());
        crate::notebook::init(cx);
        project::trusted_worktrees::init(Default::default(), cx);
        state.fs.as_fake()
    });
    fs.insert_tree(path!("/notebooks"), files).await;
    let project =
        Project::test_with_worktree_trust(fs.clone(), [path!("/notebooks").as_ref()], cx).await;
    project.read_with(cx, |project, cx| {
        assert!(project.client().status().borrow().is_signed_out());
        assert!(TrustedWorktrees::has_restricted_worktrees(
            &project.worktree_store(),
            cx
        ));
    });
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace =
        multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
    (fs, workspace, cx)
}

pub(in crate::notebook) async fn open_file(
    name: &str,
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) {
    workspace
        .update_in(cx, |workspace, window, cx| {
            let worktree_id = workspace
                .project()
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("worktree")
                .read(cx)
                .id();
            workspace.open_path((worktree_id, rel_path(name)), None, true, window, cx)
        })
        .await
        .expect("open project file");
    cx.run_until_parked();
}

#[gpui::test]
async fn test_notebook_preview_keeps_the_original_source(cx: &mut TestAppContext) {
    let (fs, workspace, cx) = notebook_workspace(json!({"field.ipynb": NOTEBOOK}), cx).await;
    open_file("field.ipynb", &workspace, cx).await;
    cx.simulate_keystrokes("x");
    workspace.read_with(cx, |workspace, cx| {
        let preview = workspace
            .active_item_as::<NotebookPreview>(cx)
            .expect("notebook preview");
        let cells = preview
            .read(cx)
            .notebook
            .as_ref()
            .expect("editable notebook")
            .read(cx);
        assert_eq!(
            cells.cell(0).expect("Markdown").current_source(cx),
            "# Field notes\n![sample](attachment:sample.png)"
        );
        assert_eq!(
            cells.cell(1).expect("code").current_source(cx),
            "print('from source')"
        );
        assert!(cells.cell(2).is_none());
        assert!(!workspace.active_item(cx).expect("notebook").is_dirty(cx));
    });
    cx.dispatch_action(workspace::Save { save_intent: None });
    cx.dispatch_action(workspace::pane::CloseActiveItem::default());
    assert!(workspace.read_with(cx, |workspace, cx| workspace.active_item(cx).is_none()));
    open_file("field.ipynb", &workspace, cx).await;
    cx.dispatch_action(EditSource);
    let edited = NOTEBOOK.replace("Field notes", "Unsaved notes");
    let buffer_id = workspace.update_in(cx, |workspace, window, cx| {
        let editor = workspace
            .active_item_as::<Editor>(cx)
            .expect("source editor");
        editor.update(cx, |editor, cx| {
            assert_eq!(editor.text(cx), NOTEBOOK);
            editor.set_text(edited.clone(), window, cx);
            editor
                .buffer()
                .read(cx)
                .as_singleton()
                .expect("source buffer")
                .entity_id()
        })
    });
    cx.update(|window, cx| {
        let source = workspace
            .read(cx)
            .active_item(cx)
            .expect("active source tab");
        let (_, action) = source
            .tab_extra_context_menu_actions(window, cx)
            .into_iter()
            .find(|(_, action)| action.partial_eq(&OpenNotebook))
            .expect("Open Notebook Editor in the source tab menu");
        window.dispatch_action(action, cx);
    });
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, cx| {
        let preview = workspace
            .active_item_as::<NotebookPreview>(cx)
            .expect("notebook preview");
        let notebook = preview
            .read(cx)
            .notebook
            .as_ref()
            .expect("editable notebook");
        assert!(
            notebook
                .read(cx)
                .cell(0)
                .expect("Markdown")
                .current_source(cx)
                .contains("Unsaved notes")
        );
    });
    cx.dispatch_action(EditSource);
    workspace.read_with(cx, |workspace, cx| {
        let editor = workspace
            .active_item_as::<Editor>(cx)
            .expect("source editor");
        assert!(editor.is_dirty(cx));
        let editor = editor.read(cx);
        let buffer = editor
            .buffer()
            .read(cx)
            .as_singleton()
            .expect("source buffer");
        assert_eq!(buffer.entity_id(), buffer_id);
        assert_eq!(editor.text(cx), edited);
    });
    assert_eq!(
        fs.load_bytes(Path::new(path!("/notebooks/field.ipynb")))
            .await
            .expect("read notebook"),
        NOTEBOOK.as_bytes(),
    );
}

#[gpui::test]
async fn test_edit_and_save_cells_in_restricted_mode(cx: &mut TestAppContext) {
    let (fs, workspace, cx) = notebook_workspace(json!({"field.ipynb": NOTEBOOK}), cx).await;
    open_file("field.ipynb", &workspace, cx).await;
    cx.dispatch_action(EditSource);
    cx.dispatch_action(OpenNotebook);
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, cx| {
        assert_eq!(workspace.active_pane().read(cx).items_len(), 1);
    });
    let view = workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_item_as::<NotebookPreview>(cx)
            .expect("notebook tab")
    });
    for (index, replacement) in [(0, "# Updated notes\n"), (1, "print('revised')\n")] {
        let editor = view.read_with(cx, |view, cx| {
            view.notebook
                .as_ref()
                .expect("notebook")
                .read(cx)
                .cell(index)
                .expect("cell")
                .editor(cx)
                .expect("editable")
                .clone()
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.set_text(replacement, window, cx);
        });
        cx.run_until_parked();
    }
    assert!(view.read_with(cx, |view, cx| workspace::Item::is_dirty(view, cx)));
    assert_eq!(
        fs.load_bytes(Path::new(path!("/notebooks/field.ipynb")))
            .await
            .expect("read file"),
        NOTEBOOK.as_bytes()
    );
    cx.dispatch_action(workspace::SaveAll { save_intent: None });
    cx.run_until_parked();
    let saved = fs
        .load_bytes(Path::new(path!("/notebooks/field.ipynb")))
        .await
        .expect("read file");
    let mut expected: Value = serde_json::from_str(NOTEBOOK).expect("fixture");
    expected["cells"][0]["source"] = json!(["# Updated notes\n"]);
    expected["cells"][1]["source"] = json!(["print('revised')\n"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&saved).expect("saved notebook"),
        expected
    );
    workspace.read_with(cx, |workspace, cx| {
        assert_eq!(
            workspace.active_item_as::<NotebookPreview>(cx),
            Some(view.clone())
        );
        assert!(!view.is_dirty(cx));
    });
    cx.dispatch_action(EditSource);
    workspace.read_with(cx, |workspace, cx| {
        let source = workspace
            .active_item_as::<Editor>(cx)
            .expect("source editor");
        assert_eq!(source.read(cx).text(cx).as_bytes(), saved);
    });
}

#[gpui::test]
async fn test_unpreviewable_files_open_in_the_editor(cx: &mut TestAppContext) {
    let malformed = "{not valid JSON";
    let unsupported = r#"{"nbformat":99,"nbformat_minor":0,"cells":[],"metadata":{}}"#;
    let invalid_heading = r#"{"nbformat":3,"worksheets":[{"cells":[{"cell_type":"heading","level":7,"metadata":{},"source":"Title"}]}]}"#;
    let (_, workspace, cx) = notebook_workspace(
        json!({"broken.ipynb": malformed, "future.ipynb": unsupported, "heading.ipynb": invalid_heading, "document.json": NOTEBOOK}),
        cx,
    )
    .await;
    for (name, source) in [
        ("broken.ipynb", malformed),
        ("future.ipynb", unsupported),
        ("heading.ipynb", invalid_heading),
        ("document.json", NOTEBOOK),
    ] {
        open_file(name, &workspace, cx).await;
        let editor_id = workspace.read_with(cx, |workspace, cx| {
            let editor = workspace.active_item_as::<Editor>(cx).expect("raw editor");
            assert_eq!(editor.read(cx).text(cx), source);
            editor.entity_id()
        });
        cx.dispatch_action(OpenNotebook);
        workspace.read_with(cx, |workspace, cx| {
            let editor = workspace
                .active_item_as::<Editor>(cx)
                .expect("still editing raw source");
            assert_eq!(editor.entity_id(), editor_id);
        });
    }
    open_file("broken.ipynb", &workspace, cx).await;
    workspace.update_in(cx, |workspace, window, cx| {
        let editor = workspace.active_item_as::<Editor>(cx).expect("raw editor");
        editor.update(cx, |editor, cx| editor.set_text(NOTEBOOK, window, cx));
    });
    cx.dispatch_action(OpenNotebook);
    workspace.read_with(cx, |workspace, cx| {
        assert!(workspace.active_item_as::<NotebookPreview>(cx).is_some());
    });
}
