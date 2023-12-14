use crate::{
    history::SearchHistory, mode::SearchMode, ActivateRegexMode, ActivateSemanticMode,
    ActivateTextMode, CycleMode, NextHistoryQuery, PreviousHistoryQuery, ReplaceAll, ReplaceNext,
    SearchOptions, SelectNextMatch, SelectPrevMatch, ToggleCaseSensitive, ToggleIncludeIgnored,
    ToggleReplace, ToggleWholeWord,
};
use anyhow::{Context as _, Result};
use collections::HashMap;
use editor::{
    items::active_match_index, scroll::autoscroll::Autoscroll, Anchor, Editor, EditorEvent,
    MultiBuffer, SelectAll, MAX_TAB_TITLE_LEN,
};
use editor::{EditorElement, EditorStyle};
use gpui::{
    actions, div, AnyElement, AnyView, AppContext, Context as _, Div, Element, EntityId,
    EventEmitter, FocusableView, FontStyle, FontWeight, InteractiveElement, IntoElement,
    KeyContext, Model, ModelContext, ParentElement, PromptLevel, Render, SharedString, Styled,
    Subscription, Task, TextStyle, View, ViewContext, VisualContext, WeakModel, WeakView,
    WhiteSpace, WindowContext,
};
use menu::Confirm;
use project::{
    search::{SearchInputs, SearchQuery},
    Entry, Project,
};
use semantic_index::{SemanticIndex, SemanticIndexStatus};

use settings::Settings;
use smol::stream::StreamExt;
use std::{
    any::{Any, TypeId},
    collections::HashSet,
    mem,
    ops::{Not, Range},
    path::PathBuf,
    time::{Duration, Instant},
};
use theme::ThemeSettings;

use ui::{
    h_stack, prelude::*, v_stack, Button, Icon, IconButton, IconElement, Label, LabelCommon,
    LabelSize, Selectable, Tooltip,
};
use util::{paths::PathMatcher, ResultExt as _};
use workspace::{
    item::{BreadcrumbText, Item, ItemEvent, ItemHandle},
    searchable::{Direction, SearchableItem, SearchableItemHandle},
    ItemNavHistory, Pane, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace,
    WorkspaceId,
};

actions!(
    project_search,
    [SearchInNew, ToggleFocus, NextField, ToggleFilters]
);

#[derive(Default)]
struct ActiveSearches(HashMap<WeakModel<Project>, WeakView<ProjectSearchView>>);

#[derive(Default)]
struct ActiveSettings(HashMap<WeakModel<Project>, ProjectSearchSettings>);

pub fn init(cx: &mut AppContext) {
    // todo!() po
    cx.set_global(ActiveSearches::default());
    cx.set_global(ActiveSettings::default());
    cx.observe_new_views(|workspace: &mut Workspace, _cx| {
        workspace
            .register_action(ProjectSearchView::deploy)
            .register_action(ProjectSearchBar::search_in_new);
    })
    .detach();
}

struct ProjectSearch {
    project: Model<Project>,
    excerpts: Model<MultiBuffer>,
    pending_search: Option<Task<Option<()>>>,
    match_ranges: Vec<Range<Anchor>>,
    active_query: Option<SearchQuery>,
    search_id: usize,
    search_history: SearchHistory,
    no_results: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum InputPanel {
    Query,
    Exclude,
    Include,
}

pub struct ProjectSearchView {
    model: Model<ProjectSearch>,
    query_editor: View<Editor>,
    replacement_editor: View<Editor>,
    results_editor: View<Editor>,
    semantic_state: Option<SemanticState>,
    semantic_permissioned: Option<bool>,
    search_options: SearchOptions,
    panels_with_errors: HashSet<InputPanel>,
    active_match_index: Option<usize>,
    search_id: usize,
    query_editor_was_focused: bool,
    included_files_editor: View<Editor>,
    excluded_files_editor: View<Editor>,
    filters_enabled: bool,
    replace_enabled: bool,
    current_mode: SearchMode,
}

struct SemanticState {
    index_status: SemanticIndexStatus,
    maintain_rate_limit: Option<Task<()>>,
    _subscription: Subscription,
}

#[derive(Debug, Clone)]
struct ProjectSearchSettings {
    search_options: SearchOptions,
    filters_enabled: bool,
    current_mode: SearchMode,
}

pub struct ProjectSearchBar {
    active_project_search: Option<View<ProjectSearchView>>,
    subscription: Option<Subscription>,
}

impl ProjectSearch {
    fn new(project: Model<Project>, cx: &mut ModelContext<Self>) -> Self {
        let replica_id = project.read(cx).replica_id();
        Self {
            project,
            excerpts: cx.build_model(|_| MultiBuffer::new(replica_id)),
            pending_search: Default::default(),
            match_ranges: Default::default(),
            active_query: None,
            search_id: 0,
            search_history: SearchHistory::default(),
            no_results: None,
        }
    }

    fn clone(&self, cx: &mut ModelContext<Self>) -> Model<Self> {
        cx.build_model(|cx| Self {
            project: self.project.clone(),
            excerpts: self
                .excerpts
                .update(cx, |excerpts, cx| cx.build_model(|cx| excerpts.clone(cx))),
            pending_search: Default::default(),
            match_ranges: self.match_ranges.clone(),
            active_query: self.active_query.clone(),
            search_id: self.search_id,
            search_history: self.search_history.clone(),
            no_results: self.no_results.clone(),
        })
    }

    fn search(&mut self, query: SearchQuery, cx: &mut ModelContext<Self>) {
        let search = self
            .project
            .update(cx, |project, cx| project.search(query.clone(), cx));
        self.search_id += 1;
        self.search_history.add(query.as_str().to_string());
        self.active_query = Some(query);
        self.match_ranges.clear();
        self.pending_search = Some(cx.spawn(|this, mut cx| async move {
            let mut matches = search;
            let this = this.upgrade()?;
            this.update(&mut cx, |this, cx| {
                this.match_ranges.clear();
                this.excerpts.update(cx, |this, cx| this.clear(cx));
                this.no_results = Some(true);
            })
            .ok()?;

            while let Some((buffer, anchors)) = matches.next().await {
                let mut ranges = this
                    .update(&mut cx, |this, cx| {
                        this.no_results = Some(false);
                        this.excerpts.update(cx, |excerpts, cx| {
                            excerpts.stream_excerpts_with_context_lines(buffer, anchors, 1, cx)
                        })
                    })
                    .ok()?;

                while let Some(range) = ranges.next().await {
                    this.update(&mut cx, |this, _| this.match_ranges.push(range))
                        .ok()?;
                }
                this.update(&mut cx, |_, cx| cx.notify()).ok()?;
            }

            this.update(&mut cx, |this, cx| {
                this.pending_search.take();
                cx.notify();
            })
            .ok()?;

            None
        }));
        cx.notify();
    }

    fn semantic_search(&mut self, inputs: &SearchInputs, cx: &mut ModelContext<Self>) {
        let search = SemanticIndex::global(cx).map(|index| {
            index.update(cx, |semantic_index, cx| {
                semantic_index.search_project(
                    self.project.clone(),
                    inputs.as_str().to_owned(),
                    10,
                    inputs.files_to_include().to_vec(),
                    inputs.files_to_exclude().to_vec(),
                    cx,
                )
            })
        });
        self.search_id += 1;
        self.match_ranges.clear();
        self.search_history.add(inputs.as_str().to_string());
        self.no_results = None;
        self.pending_search = Some(cx.spawn(|this, mut cx| async move {
            let results = search?.await.log_err()?;
            let matches = results
                .into_iter()
                .map(|result| (result.buffer, vec![result.range.start..result.range.start]));

            this.update(&mut cx, |this, cx| {
                this.no_results = Some(true);
                this.excerpts.update(cx, |excerpts, cx| {
                    excerpts.clear(cx);
                });
            })
            .ok()?;
            for (buffer, ranges) in matches {
                let mut match_ranges = this
                    .update(&mut cx, |this, cx| {
                        this.no_results = Some(false);
                        this.excerpts.update(cx, |excerpts, cx| {
                            excerpts.stream_excerpts_with_context_lines(buffer, ranges, 3, cx)
                        })
                    })
                    .ok()?;
                while let Some(match_range) = match_ranges.next().await {
                    this.update(&mut cx, |this, cx| {
                        this.match_ranges.push(match_range);
                        while let Ok(Some(match_range)) = match_ranges.try_next() {
                            this.match_ranges.push(match_range);
                        }
                        cx.notify();
                    })
                    .ok()?;
                }
            }

            this.update(&mut cx, |this, cx| {
                this.pending_search.take();
                cx.notify();
            })
            .ok()?;

            None
        }));
        cx.notify();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewEvent {
    UpdateTab,
    Activate,
    EditorEvent(editor::EditorEvent),
    Dismiss,
}

impl EventEmitter<ViewEvent> for ProjectSearchView {}

impl Render for ProjectSearchView {
    type Element = Div;
    fn render(&mut self, cx: &mut ViewContext<Self>) -> Self::Element {
        if self.has_matches() {
            div()
                .flex_1()
                .size_full()
                .child(self.results_editor.clone())
        } else {
            let model = self.model.read(cx);
            let has_no_results = model.no_results.unwrap_or(false);
            let is_search_underway = model.pending_search.is_some();
            let mut major_text = if is_search_underway {
                Label::new("Searching...")
            } else if has_no_results {
                Label::new("No results")
            } else {
                Label::new(format!("{} search all files", self.current_mode.label()))
            };

            let mut show_minor_text = true;
            let semantic_status = self.semantic_state.as_ref().and_then(|semantic| {
                let status = semantic.index_status;
                                match status {
                                    SemanticIndexStatus::NotAuthenticated => {
                                        major_text = Label::new("Not Authenticated");
                                        show_minor_text = false;
                                        Some(
                                            "API Key Missing: Please set 'OPENAI_API_KEY' in Environment Variables. If you authenticated using the Assistant Panel, please restart Zed to Authenticate.".to_string())
                                    }
                                    SemanticIndexStatus::Indexed => Some("Indexing complete".to_string()),
                                    SemanticIndexStatus::Indexing {
                                        remaining_files,
                                        rate_limit_expiry,
                                    } => {
                                        if remaining_files == 0 {
                                            Some("Indexing...".to_string())
                                        } else {
                                            if let Some(rate_limit_expiry) = rate_limit_expiry {
                                                let remaining_seconds =
                                                    rate_limit_expiry.duration_since(Instant::now());
                                                if remaining_seconds > Duration::from_secs(0) {
                                                    Some(format!(
                                                        "Remaining files to index (rate limit resets in {}s): {}",
                                                        remaining_seconds.as_secs(),
                                                        remaining_files
                                                    ))
                                                } else {
                                                    Some(format!("Remaining files to index: {}", remaining_files))
                                                }
                                            } else {
                                                Some(format!("Remaining files to index: {}", remaining_files))
                                            }
                                        }
                                    }
                                    SemanticIndexStatus::NotIndexed => None,
                                }
            });
            let major_text = div().justify_center().max_w_96().child(major_text);

            let minor_text: Option<SharedString> = if let Some(no_results) = model.no_results {
                if model.pending_search.is_none() && no_results {
                    Some("No results found in this project for the provided query".into())
                } else {
                    None
                }
            } else {
                if let Some(mut semantic_status) = semantic_status {
                    semantic_status.extend(self.landing_text_minor().chars());
                    Some(semantic_status.into())
                } else {
                    Some(self.landing_text_minor())
                }
            };
            let minor_text = minor_text.map(|text| {
                div()
                    .items_center()
                    .max_w_96()
                    .child(Label::new(text).size(LabelSize::Small))
            });
            v_stack().flex_1().size_full().justify_center().child(
                h_stack()
                    .size_full()
                    .justify_center()
                    .child(h_stack().flex_1())
                    .child(v_stack().child(major_text).children(minor_text))
                    .child(h_stack().flex_1()),
            )
        }
    }
}

impl FocusableView for ProjectSearchView {
    fn focus_handle(&self, cx: &AppContext) -> gpui::FocusHandle {
        self.results_editor.focus_handle(cx)
    }
}

impl Item for ProjectSearchView {
    type Event = ViewEvent;
    fn tab_tooltip_text(&self, cx: &AppContext) -> Option<SharedString> {
        let query_text = self.query_editor.read(cx).text(cx);

        query_text
            .is_empty()
            .not()
            .then(|| query_text.into())
            .or_else(|| Some("Project Search".into()))
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a View<Self>,
        _: &'a AppContext,
    ) -> Option<AnyView> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.results_editor.clone().into())
        } else {
            None
        }
    }

    fn deactivated(&mut self, cx: &mut ViewContext<Self>) {
        self.results_editor
            .update(cx, |editor, cx| editor.deactivated(cx));
    }

    fn tab_content(&self, _: Option<usize>, selected: bool, cx: &WindowContext<'_>) -> AnyElement {
        let last_query: Option<SharedString> = self
            .model
            .read(cx)
            .search_history
            .current()
            .as_ref()
            .map(|query| {
                let query_text = util::truncate_and_trailoff(query, MAX_TAB_TITLE_LEN);
                query_text.into()
            });
        let tab_name = last_query
            .filter(|query| !query.is_empty())
            .unwrap_or_else(|| "Project search".into());
        h_stack()
            .gap_2()
            .child(IconElement::new(Icon::MagnifyingGlass).color(if selected {
                Color::Default
            } else {
                Color::Muted
            }))
            .child(Label::new(tab_name).color(if selected {
                Color::Default
            } else {
                Color::Muted
            }))
            .into_any()
    }

    fn for_each_project_item(
        &self,
        cx: &AppContext,
        f: &mut dyn FnMut(EntityId, &dyn project::Item),
    ) {
        self.results_editor.for_each_project_item(cx, f)
    }

    fn is_singleton(&self, _: &AppContext) -> bool {
        false
    }

    fn can_save(&self, _: &AppContext) -> bool {
        true
    }

    fn is_dirty(&self, cx: &AppContext) -> bool {
        self.results_editor.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &AppContext) -> bool {
        self.results_editor.read(cx).has_conflict(cx)
    }

    fn save(
        &mut self,
        project: Model<Project>,
        cx: &mut ViewContext<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.results_editor
            .update(cx, |editor, cx| editor.save(project, cx))
    }

    fn save_as(
        &mut self,
        _: Model<Project>,
        _: PathBuf,
        _: &mut ViewContext<Self>,
    ) -> Task<anyhow::Result<()>> {
        unreachable!("save_as should not have been called")
    }

    fn reload(
        &mut self,
        project: Model<Project>,
        cx: &mut ViewContext<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.results_editor
            .update(cx, |editor, cx| editor.reload(project, cx))
    }

    fn clone_on_split(
        &self,
        _workspace_id: WorkspaceId,
        cx: &mut ViewContext<Self>,
    ) -> Option<View<Self>>
    where
        Self: Sized,
    {
        let model = self.model.update(cx, |model, cx| model.clone(cx));
        Some(cx.build_view(|cx| Self::new(model, cx, None)))
    }

    fn added_to_workspace(&mut self, workspace: &mut Workspace, cx: &mut ViewContext<Self>) {
        self.results_editor
            .update(cx, |editor, cx| editor.added_to_workspace(workspace, cx));
    }

    fn set_nav_history(&mut self, nav_history: ItemNavHistory, cx: &mut ViewContext<Self>) {
        self.results_editor.update(cx, |editor, _| {
            editor.set_nav_history(Some(nav_history));
        });
    }

    fn navigate(&mut self, data: Box<dyn Any>, cx: &mut ViewContext<Self>) -> bool {
        self.results_editor
            .update(cx, |editor, cx| editor.navigate(data, cx))
    }

    fn to_item_events(event: &Self::Event, mut f: impl FnMut(ItemEvent)) {
        match event {
            ViewEvent::UpdateTab => {
                f(ItemEvent::UpdateBreadcrumbs);
                f(ItemEvent::UpdateTab);
            }
            ViewEvent::EditorEvent(editor_event) => {
                Editor::to_item_events(editor_event, f);
            }
            ViewEvent::Dismiss => f(ItemEvent::CloseItem),
            _ => {}
        }
    }

    fn breadcrumb_location(&self) -> ToolbarItemLocation {
        if self.has_matches() {
            ToolbarItemLocation::Secondary
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn breadcrumbs(&self, theme: &theme::Theme, cx: &AppContext) -> Option<Vec<BreadcrumbText>> {
        self.results_editor.breadcrumbs(theme, cx)
    }

    fn serialized_item_kind() -> Option<&'static str> {
        None
    }

    fn deserialize(
        _project: Model<Project>,
        _workspace: WeakView<Workspace>,
        _workspace_id: workspace::WorkspaceId,
        _item_id: workspace::ItemId,
        _cx: &mut ViewContext<Pane>,
    ) -> Task<anyhow::Result<View<Self>>> {
        unimplemented!()
    }
}

impl ProjectSearchView {
    fn toggle_filters(&mut self, cx: &mut ViewContext<Self>) {
        self.filters_enabled = !self.filters_enabled;
        cx.update_global(|state: &mut ActiveSettings, cx| {
            state.0.insert(
                self.model.read(cx).project.downgrade(),
                self.current_settings(),
            );
        });
    }

    fn current_settings(&self) -> ProjectSearchSettings {
        ProjectSearchSettings {
            search_options: self.search_options,
            filters_enabled: self.filters_enabled,
            current_mode: self.current_mode,
        }
    }
    fn toggle_search_option(&mut self, option: SearchOptions, cx: &mut ViewContext<Self>) {
        self.search_options.toggle(option);
        cx.update_global(|state: &mut ActiveSettings, cx| {
            state.0.insert(
                self.model.read(cx).project.downgrade(),
                self.current_settings(),
            );
        });
    }

    fn index_project(&mut self, cx: &mut ViewContext<Self>) {
        if let Some(semantic_index) = SemanticIndex::global(cx) {
            // Semantic search uses no options
            self.search_options = SearchOptions::none();

            let project = self.model.read(cx).project.clone();

            semantic_index.update(cx, |semantic_index, cx| {
                semantic_index
                    .index_project(project.clone(), cx)
                    .detach_and_log_err(cx);
            });

            self.semantic_state = Some(SemanticState {
                index_status: semantic_index.read(cx).status(&project),
                maintain_rate_limit: None,
                _subscription: cx.observe(&semantic_index, Self::semantic_index_changed),
            });
            self.semantic_index_changed(semantic_index, cx);
        }
    }

    fn semantic_index_changed(
        &mut self,
        semantic_index: Model<SemanticIndex>,
        cx: &mut ViewContext<Self>,
    ) {
        let project = self.model.read(cx).project.clone();
        if let Some(semantic_state) = self.semantic_state.as_mut() {
            cx.notify();
            semantic_state.index_status = semantic_index.read(cx).status(&project);
            if let SemanticIndexStatus::Indexing {
                rate_limit_expiry: Some(_),
                ..
            } = &semantic_state.index_status
            {
                if semantic_state.maintain_rate_limit.is_none() {
                    semantic_state.maintain_rate_limit =
                        Some(cx.spawn(|this, mut cx| async move {
                            loop {
                                cx.background_executor().timer(Duration::from_secs(1)).await;
                                this.update(&mut cx, |_, cx| cx.notify()).log_err();
                            }
                        }));
                    return;
                }
            } else {
                semantic_state.maintain_rate_limit = None;
            }
        }
    }

    fn clear_search(&mut self, cx: &mut ViewContext<Self>) {
        self.model.update(cx, |model, cx| {
            model.pending_search = None;
            model.no_results = None;
            model.match_ranges.clear();

            model.excerpts.update(cx, |excerpts, cx| {
                excerpts.clear(cx);
            });
        });
    }

    fn activate_search_mode(&mut self, mode: SearchMode, cx: &mut ViewContext<Self>) {
        let previous_mode = self.current_mode;
        if previous_mode == mode {
            return;
        }

        self.clear_search(cx);
        self.current_mode = mode;
        self.active_match_index = None;

        match mode {
            SearchMode::Semantic => {
                let has_permission = self.semantic_permissioned(cx);
                self.active_match_index = None;
                cx.spawn(|this, mut cx| async move {
                    let has_permission = has_permission.await?;

                    if !has_permission {
                        let answer = this.update(&mut cx, |this, cx| {
                            let project = this.model.read(cx).project.clone();
                            let project_name = project
                                .read(cx)
                                .worktree_root_names(cx)
                                .collect::<Vec<&str>>()
                                .join("/");
                            let is_plural =
                                project_name.chars().filter(|letter| *letter == '/').count() > 0;
                            let prompt_text = format!("Would you like to index the '{}' project{} for semantic search? This requires sending code to the OpenAI API", project_name,
                                if is_plural {
                                    "s"
                                } else {""});
                            cx.prompt(
                                PromptLevel::Info,
                                prompt_text.as_str(),
                                &["Continue", "Cancel"],
                            )
                        })?;

                        if answer.await? == 0 {
                            this.update(&mut cx, |this, _| {
                                this.semantic_permissioned = Some(true);
                            })?;
                        } else {
                            this.update(&mut cx, |this, cx| {
                                this.semantic_permissioned = Some(false);
                                debug_assert_ne!(previous_mode, SearchMode::Semantic, "Tried to re-enable semantic search mode after user modal was rejected");
                                this.activate_search_mode(previous_mode, cx);
                            })?;
                            return anyhow::Ok(());
                        }
                    }

                    this.update(&mut cx, |this, cx| {
                        this.index_project(cx);
                    })?;

                    anyhow::Ok(())
                }).detach_and_log_err(cx);
            }
            SearchMode::Regex | SearchMode::Text => {
                self.semantic_state = None;
                self.active_match_index = None;
                self.search(cx);
            }
        }

        cx.update_global(|state: &mut ActiveSettings, cx| {
            state.0.insert(
                self.model.read(cx).project.downgrade(),
                self.current_settings(),
            );
        });

        cx.notify();
    }
    fn replace_next(&mut self, _: &ReplaceNext, cx: &mut ViewContext<Self>) {
        let model = self.model.read(cx);
        if let Some(query) = model.active_query.as_ref() {
            if model.match_ranges.is_empty() {
                return;
            }
            if let Some(active_index) = self.active_match_index {
                let query = query.clone().with_replacement(self.replacement(cx));
                self.results_editor.replace(
                    &(Box::new(model.match_ranges[active_index].clone()) as _),
                    &query,
                    cx,
                );
                self.select_match(Direction::Next, cx)
            }
        }
    }
    pub fn replacement(&self, cx: &AppContext) -> String {
        self.replacement_editor.read(cx).text(cx)
    }
    fn replace_all(&mut self, _: &ReplaceAll, cx: &mut ViewContext<Self>) {
        let model = self.model.read(cx);
        if let Some(query) = model.active_query.as_ref() {
            if model.match_ranges.is_empty() {
                return;
            }
            if self.active_match_index.is_some() {
                let query = query.clone().with_replacement(self.replacement(cx));
                let matches = model
                    .match_ranges
                    .iter()
                    .map(|item| Box::new(item.clone()) as _)
                    .collect::<Vec<_>>();
                for item in matches {
                    self.results_editor.replace(&item, &query, cx);
                }
            }
        }
    }

    fn new(
        model: Model<ProjectSearch>,
        cx: &mut ViewContext<Self>,
        settings: Option<ProjectSearchSettings>,
    ) -> Self {
        let project;
        let excerpts;
        let mut replacement_text = None;
        let mut query_text = String::new();

        // Read in settings if available
        let (mut options, current_mode, filters_enabled) = if let Some(settings) = settings {
            (
                settings.search_options,
                settings.current_mode,
                settings.filters_enabled,
            )
        } else {
            (SearchOptions::NONE, Default::default(), false)
        };

        {
            let model = model.read(cx);
            project = model.project.clone();
            excerpts = model.excerpts.clone();
            if let Some(active_query) = model.active_query.as_ref() {
                query_text = active_query.as_str().to_string();
                replacement_text = active_query.replacement().map(ToOwned::to_owned);
                options = SearchOptions::from_query(active_query);
            }
        }
        cx.observe(&model, |this, _, cx| this.model_changed(cx))
            .detach();

        let query_editor = cx.build_view(|cx| {
            let mut editor = Editor::single_line(cx);
            editor.set_placeholder_text("Text search all files", cx);
            editor.set_text(query_text, cx);
            editor
        });
        // Subscribe to query_editor in order to reraise editor events for workspace item activation purposes
        cx.subscribe(&query_editor, |_, _, event: &EditorEvent, cx| {
            cx.emit(ViewEvent::EditorEvent(event.clone()))
        })
        .detach();
        let replacement_editor = cx.build_view(|cx| {
            let mut editor = Editor::single_line(cx);
            editor.set_placeholder_text("Replace in project..", cx);
            if let Some(text) = replacement_text {
                editor.set_text(text, cx);
            }
            editor
        });
        let results_editor = cx.build_view(|cx| {
            let mut editor = Editor::for_multibuffer(excerpts, Some(project.clone()), cx);
            editor.set_searchable(false);
            editor
        });
        cx.observe(&results_editor, |_, _, cx| cx.emit(ViewEvent::UpdateTab))
            .detach();

        cx.subscribe(&results_editor, |this, _, event: &EditorEvent, cx| {
            if matches!(event, editor::EditorEvent::SelectionsChanged { .. }) {
                this.update_match_index(cx);
            }
            // Reraise editor events for workspace item activation purposes
            cx.emit(ViewEvent::EditorEvent(event.clone()));
        })
        .detach();

        let included_files_editor = cx.build_view(|cx| {
            let mut editor = Editor::single_line(cx);
            editor.set_placeholder_text("Include: crates/**/*.toml", cx);

            editor
        });
        // Subscribe to include_files_editor in order to reraise editor events for workspace item activation purposes
        cx.subscribe(&included_files_editor, |_, _, event: &EditorEvent, cx| {
            cx.emit(ViewEvent::EditorEvent(event.clone()))
        })
        .detach();

        let excluded_files_editor = cx.build_view(|cx| {
            let mut editor = Editor::single_line(cx);
            editor.set_placeholder_text("Exclude: vendor/*, *.lock", cx);

            editor
        });
        // Subscribe to excluded_files_editor in order to reraise editor events for workspace item activation purposes
        cx.subscribe(&excluded_files_editor, |_, _, event: &EditorEvent, cx| {
            cx.emit(ViewEvent::EditorEvent(event.clone()))
        })
        .detach();

        // Check if Worktrees have all been previously indexed
        let mut this = ProjectSearchView {
            replacement_editor,
            search_id: model.read(cx).search_id,
            model,
            query_editor,
            results_editor,
            semantic_state: None,
            semantic_permissioned: None,
            search_options: options,
            panels_with_errors: HashSet::new(),
            active_match_index: None,
            query_editor_was_focused: false,
            included_files_editor,
            excluded_files_editor,
            filters_enabled,
            current_mode,
            replace_enabled: false,
        };
        this.model_changed(cx);
        this
    }

    fn semantic_permissioned(&mut self, cx: &mut ViewContext<Self>) -> Task<Result<bool>> {
        if let Some(value) = self.semantic_permissioned {
            return Task::ready(Ok(value));
        }

        SemanticIndex::global(cx)
            .map(|semantic| {
                let project = self.model.read(cx).project.clone();
                semantic.update(cx, |this, cx| this.project_previously_indexed(&project, cx))
            })
            .unwrap_or(Task::ready(Ok(false)))
    }
    pub fn new_search_in_directory(
        workspace: &mut Workspace,
        dir_entry: &Entry,
        cx: &mut ViewContext<Workspace>,
    ) {
        if !dir_entry.is_dir() {
            return;
        }
        let Some(filter_str) = dir_entry.path.to_str() else {
            return;
        };

        let model = cx.build_model(|cx| ProjectSearch::new(workspace.project().clone(), cx));
        let search = cx.build_view(|cx| ProjectSearchView::new(model, cx, None));
        workspace.add_item(Box::new(search.clone()), cx);
        search.update(cx, |search, cx| {
            search
                .included_files_editor
                .update(cx, |editor, cx| editor.set_text(filter_str, cx));
            search.filters_enabled = true;
            search.focus_query_editor(cx)
        });
    }

    // Add another search tab to the workspace.
    fn deploy(
        workspace: &mut Workspace,
        _: &workspace::NewSearch,
        cx: &mut ViewContext<Workspace>,
    ) {
        // Clean up entries for dropped projects
        cx.update_global(|state: &mut ActiveSearches, _cx| {
            state.0.retain(|project, _| project.is_upgradable())
        });

        let query = workspace.active_item(cx).and_then(|item| {
            let editor = item.act_as::<Editor>(cx)?;
            let query = editor.query_suggestion(cx);
            if query.is_empty() {
                None
            } else {
                Some(query)
            }
        });

        let settings = cx
            .global::<ActiveSettings>()
            .0
            .get(&workspace.project().downgrade());

        let settings = if let Some(settings) = settings {
            Some(settings.clone())
        } else {
            None
        };

        let model = cx.build_model(|cx| ProjectSearch::new(workspace.project().clone(), cx));
        let search = cx.build_view(|cx| ProjectSearchView::new(model, cx, settings));

        workspace.add_item(Box::new(search.clone()), cx);

        search.update(cx, |search, cx| {
            if let Some(query) = query {
                search.set_query(&query, cx);
            }
            search.focus_query_editor(cx)
        });
    }

    fn search(&mut self, cx: &mut ViewContext<Self>) {
        let mode = self.current_mode;
        match mode {
            SearchMode::Semantic => {
                if self.semantic_state.is_some() {
                    if let Some(query) = self.build_search_query(cx) {
                        self.model
                            .update(cx, |model, cx| model.semantic_search(query.as_inner(), cx));
                    }
                }
            }

            _ => {
                if let Some(query) = self.build_search_query(cx) {
                    self.model.update(cx, |model, cx| model.search(query, cx));
                }
            }
        }
    }

    fn build_search_query(&mut self, cx: &mut ViewContext<Self>) -> Option<SearchQuery> {
        let text = self.query_editor.read(cx).text(cx);
        let included_files =
            match Self::parse_path_matches(&self.included_files_editor.read(cx).text(cx)) {
                Ok(included_files) => {
                    self.panels_with_errors.remove(&InputPanel::Include);
                    included_files
                }
                Err(_e) => {
                    self.panels_with_errors.insert(InputPanel::Include);
                    cx.notify();
                    return None;
                }
            };
        let excluded_files =
            match Self::parse_path_matches(&self.excluded_files_editor.read(cx).text(cx)) {
                Ok(excluded_files) => {
                    self.panels_with_errors.remove(&InputPanel::Exclude);
                    excluded_files
                }
                Err(_e) => {
                    self.panels_with_errors.insert(InputPanel::Exclude);
                    cx.notify();
                    return None;
                }
            };
        let current_mode = self.current_mode;
        match current_mode {
            SearchMode::Regex => {
                match SearchQuery::regex(
                    text,
                    self.search_options.contains(SearchOptions::WHOLE_WORD),
                    self.search_options.contains(SearchOptions::CASE_SENSITIVE),
                    self.search_options.contains(SearchOptions::INCLUDE_IGNORED),
                    included_files,
                    excluded_files,
                ) {
                    Ok(query) => {
                        self.panels_with_errors.remove(&InputPanel::Query);
                        Some(query)
                    }
                    Err(_e) => {
                        self.panels_with_errors.insert(InputPanel::Query);
                        cx.notify();
                        None
                    }
                }
            }
            _ => match SearchQuery::text(
                text,
                self.search_options.contains(SearchOptions::WHOLE_WORD),
                self.search_options.contains(SearchOptions::CASE_SENSITIVE),
                self.search_options.contains(SearchOptions::INCLUDE_IGNORED),
                included_files,
                excluded_files,
            ) {
                Ok(query) => {
                    self.panels_with_errors.remove(&InputPanel::Query);
                    Some(query)
                }
                Err(_e) => {
                    self.panels_with_errors.insert(InputPanel::Query);
                    cx.notify();
                    None
                }
            },
        }
    }

    fn parse_path_matches(text: &str) -> anyhow::Result<Vec<PathMatcher>> {
        text.split(',')
            .map(str::trim)
            .filter(|maybe_glob_str| !maybe_glob_str.is_empty())
            .map(|maybe_glob_str| {
                PathMatcher::new(maybe_glob_str)
                    .with_context(|| format!("parsing {maybe_glob_str} as path matcher"))
            })
            .collect()
    }

    fn select_match(&mut self, direction: Direction, cx: &mut ViewContext<Self>) {
        if let Some(index) = self.active_match_index {
            let match_ranges = self.model.read(cx).match_ranges.clone();
            let new_index = self.results_editor.update(cx, |editor, cx| {
                editor.match_index_for_direction(&match_ranges, index, direction, 1, cx)
            });

            let range_to_select = match_ranges[new_index].clone();
            self.results_editor.update(cx, |editor, cx| {
                let range_to_select = editor.range_for_match(&range_to_select);
                editor.unfold_ranges([range_to_select.clone()], false, true, cx);
                editor.change_selections(Some(Autoscroll::fit()), cx, |s| {
                    s.select_ranges([range_to_select])
                });
            });
        }
    }

    fn focus_query_editor(&mut self, cx: &mut ViewContext<Self>) {
        self.query_editor.update(cx, |query_editor, cx| {
            query_editor.select_all(&SelectAll, cx);
        });
        self.query_editor_was_focused = true;
        let editor_handle = self.query_editor.focus_handle(cx);
        cx.focus(&editor_handle);
    }

    fn set_query(&mut self, query: &str, cx: &mut ViewContext<Self>) {
        self.query_editor
            .update(cx, |query_editor, cx| query_editor.set_text(query, cx));
    }

    fn focus_results_editor(&mut self, cx: &mut ViewContext<Self>) {
        self.query_editor.update(cx, |query_editor, cx| {
            let cursor = query_editor.selections.newest_anchor().head();
            query_editor.change_selections(None, cx, |s| s.select_ranges([cursor.clone()..cursor]));
        });
        self.query_editor_was_focused = false;
        let results_handle = self.results_editor.focus_handle(cx);
        cx.focus(&results_handle);
    }

    fn model_changed(&mut self, cx: &mut ViewContext<Self>) {
        let match_ranges = self.model.read(cx).match_ranges.clone();
        if match_ranges.is_empty() {
            self.active_match_index = None;
        } else {
            self.active_match_index = Some(0);
            self.update_match_index(cx);
            let prev_search_id = mem::replace(&mut self.search_id, self.model.read(cx).search_id);
            let is_new_search = self.search_id != prev_search_id;
            self.results_editor.update(cx, |editor, cx| {
                if is_new_search {
                    let range_to_select = match_ranges
                        .first()
                        .clone()
                        .map(|range| editor.range_for_match(range));
                    editor.change_selections(Some(Autoscroll::fit()), cx, |s| {
                        s.select_ranges(range_to_select)
                    });
                }
                editor.highlight_background::<Self>(
                    match_ranges,
                    |theme| theme.search_match_background,
                    cx,
                );
            });
            if is_new_search && self.query_editor.focus_handle(cx).is_focused(cx) {
                self.focus_results_editor(cx);
            }
        }

        cx.emit(ViewEvent::UpdateTab);
        cx.notify();
    }

    fn update_match_index(&mut self, cx: &mut ViewContext<Self>) {
        let results_editor = self.results_editor.read(cx);
        let new_index = active_match_index(
            &self.model.read(cx).match_ranges,
            &results_editor.selections.newest_anchor().head(),
            &results_editor.buffer().read(cx).snapshot(cx),
        );
        if self.active_match_index != new_index {
            self.active_match_index = new_index;
            cx.notify();
        }
    }

    pub fn has_matches(&self) -> bool {
        self.active_match_index.is_some()
    }

    fn landing_text_minor(&self) -> SharedString {
        match self.current_mode {
            SearchMode::Text | SearchMode::Regex => "Include/exclude specific paths with the filter option. Matching exact word and/or casing is available too.".into(),
            SearchMode::Semantic => "\nSimply explain the code you are looking to find. ex. 'prompt user for permissions to index their project'".into()
        }
    }
}

impl Default for ProjectSearchBar {
    fn default() -> Self {
        Self::new()
    }
}

impl ProjectSearchBar {
    pub fn new() -> Self {
        Self {
            active_project_search: Default::default(),
            subscription: Default::default(),
        }
    }
    fn cycle_mode(&self, _: &CycleMode, cx: &mut ViewContext<Self>) {
        if let Some(view) = self.active_project_search.as_ref() {
            view.update(cx, |this, cx| {
                let new_mode =
                    crate::mode::next_mode(&this.current_mode, SemanticIndex::enabled(cx));
                this.activate_search_mode(new_mode, cx);
                let editor_handle = this.query_editor.focus_handle(cx);
                cx.focus(&editor_handle);
            });
        }
    }
    fn confirm(&mut self, _: &Confirm, cx: &mut ViewContext<Self>) {
        if let Some(search_view) = self.active_project_search.as_ref() {
            search_view.update(cx, |search_view, cx| {
                if !search_view
                    .replacement_editor
                    .focus_handle(cx)
                    .is_focused(cx)
                {
                    cx.stop_propagation();
                    search_view.search(cx);
                }
            });
        }
    }

    fn search_in_new(workspace: &mut Workspace, _: &SearchInNew, cx: &mut ViewContext<Workspace>) {
        if let Some(search_view) = workspace
            .active_item(cx)
            .and_then(|item| item.downcast::<ProjectSearchView>())
        {
            let new_query = search_view.update(cx, |search_view, cx| {
                let new_query = search_view.build_search_query(cx);
                if new_query.is_some() {
                    if let Some(old_query) = search_view.model.read(cx).active_query.clone() {
                        search_view.query_editor.update(cx, |editor, cx| {
                            editor.set_text(old_query.as_str(), cx);
                        });
                        search_view.search_options = SearchOptions::from_query(&old_query);
                    }
                }
                new_query
            });
            if let Some(new_query) = new_query {
                let model = cx.build_model(|cx| {
                    let mut model = ProjectSearch::new(workspace.project().clone(), cx);
                    model.search(new_query, cx);
                    model
                });
                workspace.add_item(
                    Box::new(cx.build_view(|cx| ProjectSearchView::new(model, cx, None))),
                    cx,
                );
            }
        }
    }

    fn tab(&mut self, _: &editor::Tab, cx: &mut ViewContext<Self>) {
        self.cycle_field(Direction::Next, cx);
    }

    fn tab_previous(&mut self, _: &editor::TabPrev, cx: &mut ViewContext<Self>) {
        self.cycle_field(Direction::Prev, cx);
    }

    fn cycle_field(&mut self, direction: Direction, cx: &mut ViewContext<Self>) {
        let active_project_search = match &self.active_project_search {
            Some(active_project_search) => active_project_search,

            None => {
                return;
            }
        };

        active_project_search.update(cx, |project_view, cx| {
            let mut views = vec![&project_view.query_editor];
            if project_view.filters_enabled {
                views.extend([
                    &project_view.included_files_editor,
                    &project_view.excluded_files_editor,
                ]);
            }
            if project_view.replace_enabled {
                views.push(&project_view.replacement_editor);
            }
            let current_index = match views
                .iter()
                .enumerate()
                .find(|(_, view)| view.focus_handle(cx).is_focused(cx))
            {
                Some((index, _)) => index,

                None => {
                    return;
                }
            };

            let new_index = match direction {
                Direction::Next => (current_index + 1) % views.len(),
                Direction::Prev if current_index == 0 => views.len() - 1,
                Direction::Prev => (current_index - 1) % views.len(),
            };
            let next_focus_handle = views[new_index].focus_handle(cx);
            cx.focus(&next_focus_handle);
            cx.stop_propagation();
        });
    }

    fn toggle_search_option(&mut self, option: SearchOptions, cx: &mut ViewContext<Self>) -> bool {
        if let Some(search_view) = self.active_project_search.as_ref() {
            search_view.update(cx, |search_view, cx| {
                search_view.toggle_search_option(option, cx);
                search_view.search(cx);
            });

            cx.notify();
            true
        } else {
            false
        }
    }
    fn toggle_replace(&mut self, _: &ToggleReplace, cx: &mut ViewContext<Self>) {
        if let Some(search) = &self.active_project_search {
            search.update(cx, |this, cx| {
                this.replace_enabled = !this.replace_enabled;
                let editor_to_focus = if !this.replace_enabled {
                    this.query_editor.focus_handle(cx)
                } else {
                    this.replacement_editor.focus_handle(cx)
                };
                cx.focus(&editor_to_focus);
                cx.notify();
            });
        }
    }

    fn toggle_filters(&mut self, cx: &mut ViewContext<Self>) -> bool {
        if let Some(search_view) = self.active_project_search.as_ref() {
            search_view.update(cx, |search_view, cx| {
                search_view.toggle_filters(cx);
                search_view
                    .included_files_editor
                    .update(cx, |_, cx| cx.notify());
                search_view
                    .excluded_files_editor
                    .update(cx, |_, cx| cx.notify());
                cx.refresh();
                cx.notify();
            });
            cx.notify();
            true
        } else {
            false
        }
    }

    fn activate_search_mode(&self, mode: SearchMode, cx: &mut ViewContext<Self>) {
        // Update Current Mode
        if let Some(search_view) = self.active_project_search.as_ref() {
            search_view.update(cx, |search_view, cx| {
                search_view.activate_search_mode(mode, cx);
            });
            cx.notify();
        }
    }

    fn is_option_enabled(&self, option: SearchOptions, cx: &AppContext) -> bool {
        if let Some(search) = self.active_project_search.as_ref() {
            search.read(cx).search_options.contains(option)
        } else {
            false
        }
    }

    fn next_history_query(&mut self, _: &NextHistoryQuery, cx: &mut ViewContext<Self>) {
        if let Some(search_view) = self.active_project_search.as_ref() {
            search_view.update(cx, |search_view, cx| {
                let new_query = search_view.model.update(cx, |model, _| {
                    if let Some(new_query) = model.search_history.next().map(str::to_string) {
                        new_query
                    } else {
                        model.search_history.reset_selection();
                        String::new()
                    }
                });
                search_view.set_query(&new_query, cx);
            });
        }
    }

    fn previous_history_query(&mut self, _: &PreviousHistoryQuery, cx: &mut ViewContext<Self>) {
        if let Some(search_view) = self.active_project_search.as_ref() {
            search_view.update(cx, |search_view, cx| {
                if search_view.query_editor.read(cx).text(cx).is_empty() {
                    if let Some(new_query) = search_view
                        .model
                        .read(cx)
                        .search_history
                        .current()
                        .map(str::to_string)
                    {
                        search_view.set_query(&new_query, cx);
                        return;
                    }
                }

                if let Some(new_query) = search_view.model.update(cx, |model, _| {
                    model.search_history.previous().map(str::to_string)
                }) {
                    search_view.set_query(&new_query, cx);
                }
            });
        }
    }
    fn new_placeholder_text(&self, cx: &mut ViewContext<Self>) -> Option<String> {
        let previous_query_keystrokes = cx
            .bindings_for_action(&PreviousHistoryQuery {})
            .into_iter()
            .next()
            .map(|binding| {
                binding
                    .keystrokes()
                    .iter()
                    .map(|k| k.to_string())
                    .collect::<Vec<_>>()
            });
        let next_query_keystrokes = cx
            .bindings_for_action(&NextHistoryQuery {})
            .into_iter()
            .next()
            .map(|binding| {
                binding
                    .keystrokes()
                    .iter()
                    .map(|k| k.to_string())
                    .collect::<Vec<_>>()
            });
        let new_placeholder_text = match (previous_query_keystrokes, next_query_keystrokes) {
            (Some(previous_query_keystrokes), Some(next_query_keystrokes)) => Some(format!(
                "Search ({}/{} for previous/next query)",
                previous_query_keystrokes.join(" "),
                next_query_keystrokes.join(" ")
            )),
            (None, Some(next_query_keystrokes)) => Some(format!(
                "Search ({} for next query)",
                next_query_keystrokes.join(" ")
            )),
            (Some(previous_query_keystrokes), None) => Some(format!(
                "Search ({} for previous query)",
                previous_query_keystrokes.join(" ")
            )),
            (None, None) => None,
        };
        new_placeholder_text
    }

    fn render_text_input(&self, editor: &View<Editor>, cx: &ViewContext<Self>) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let text_style = TextStyle {
            color: if editor.read(cx).read_only() {
                cx.theme().colors().text_disabled
            } else {
                cx.theme().colors().text
            },
            font_family: settings.ui_font.family.clone(),
            font_features: settings.ui_font.features,
            font_size: rems(0.875).into(),
            font_weight: FontWeight::NORMAL,
            font_style: FontStyle::Normal,
            line_height: relative(1.3).into(),
            background_color: None,
            underline: None,
            white_space: WhiteSpace::Normal,
        };

        EditorElement::new(
            &editor,
            EditorStyle {
                background: cx.theme().colors().editor_background,
                local_player: cx.theme().players().local(),
                text: text_style,
                ..Default::default()
            },
        )
    }
}

impl Render for ProjectSearchBar {
    type Element = Div;

    fn render(&mut self, cx: &mut ViewContext<Self>) -> Self::Element {
        let Some(search) = self.active_project_search.clone() else {
            return div();
        };
        let mut key_context = KeyContext::default();
        key_context.add("ProjectSearchBar");
        if let Some(placeholder_text) = self.new_placeholder_text(cx) {
            search.update(cx, |search, cx| {
                search.query_editor.update(cx, |this, cx| {
                    this.set_placeholder_text(placeholder_text, cx)
                })
            });
        }
        let search = search.read(cx);
        let semantic_is_available = SemanticIndex::enabled(cx);
        let query_column = v_stack().child(
            h_stack()
                .min_w(rems(512. / 16.))
                .px_2()
                .py_1()
                .gap_2()
                .bg(cx.theme().colors().editor_background)
                .border_1()
                .border_color(cx.theme().colors().border)
                .rounded_lg()
                .on_action(cx.listener(|this, action, cx| this.confirm(action, cx)))
                .on_action(cx.listener(|this, action, cx| this.previous_history_query(action, cx)))
                .on_action(cx.listener(|this, action, cx| this.next_history_query(action, cx)))
                .child(IconElement::new(Icon::MagnifyingGlass))
                .child(self.render_text_input(&search.query_editor, cx))
                .child(
                    h_stack()
                        .child(
                            IconButton::new("project-search-filter-button", Icon::Filter)
                                .tooltip(|cx| {
                                    Tooltip::for_action("Toggle filters", &ToggleFilters, cx)
                                })
                                .on_click(cx.listener(|this, _, cx| {
                                    this.toggle_filters(cx);
                                }))
                                .selected(
                                    self.active_project_search
                                        .as_ref()
                                        .map(|search| search.read(cx).filters_enabled)
                                        .unwrap_or_default(),
                                ),
                        )
                        .when(search.current_mode != SearchMode::Semantic, |this| {
                            this.child(
                                IconButton::new(
                                    "project-search-case-sensitive",
                                    Icon::CaseSensitive,
                                )
                                .tooltip(|cx| {
                                    Tooltip::for_action(
                                        "Toggle case sensitive",
                                        &ToggleCaseSensitive,
                                        cx,
                                    )
                                })
                                .selected(self.is_option_enabled(SearchOptions::WHOLE_WORD, cx))
                                .on_click(cx.listener(
                                    |this, _, cx| {
                                        this.toggle_search_option(SearchOptions::WHOLE_WORD, cx);
                                    },
                                )),
                            )
                        }),
                ),
        );

        let mode_column = v_stack().items_start().justify_start().child(
            h_stack()
                .child(
                    h_stack()
                        .child(
                            Button::new("project-search-text-button", "Text")
                                .selected(search.current_mode == SearchMode::Text)
                                .on_click(cx.listener(|this, _, cx| {
                                    this.activate_search_mode(SearchMode::Text, cx)
                                }))
                                .tooltip(|cx| {
                                    Tooltip::for_action("Toggle text search", &ActivateTextMode, cx)
                                }),
                        )
                        .child(
                            Button::new("project-search-regex-button", "Regex")
                                .selected(search.current_mode == SearchMode::Regex)
                                .on_click(cx.listener(|this, _, cx| {
                                    this.activate_search_mode(SearchMode::Regex, cx)
                                }))
                                .tooltip(|cx| {
                                    Tooltip::for_action(
                                        "Toggle regular expression search",
                                        &ActivateRegexMode,
                                        cx,
                                    )
                                }),
                        )
                        .when(semantic_is_available, |this| {
                            this.child(
                                Button::new("project-search-semantic-button", "Semantic")
                                    .selected(search.current_mode == SearchMode::Semantic)
                                    .on_click(cx.listener(|this, _, cx| {
                                        this.activate_search_mode(SearchMode::Semantic, cx)
                                    }))
                                    .tooltip(|cx| {
                                        Tooltip::for_action(
                                            "Toggle semantic search",
                                            &ActivateSemanticMode,
                                            cx,
                                        )
                                    }),
                            )
                        }),
                )
                .child(
                    IconButton::new("project-search-toggle-replace", Icon::Replace)
                        .on_click(cx.listener(|this, _, cx| {
                            this.toggle_replace(&ToggleReplace, cx);
                        }))
                        .tooltip(|cx| Tooltip::for_action("Toggle replace", &ToggleReplace, cx)),
                ),
        );
        let replace_column = if search.replace_enabled {
            h_stack()
                .p_1()
                .flex_1()
                .border_2()
                .rounded_lg()
                .child(IconElement::new(Icon::Replace).size(ui::IconSize::Small))
                .child(search.replacement_editor.clone())
        } else {
            // Fill out the space if we don't have a replacement editor.
            h_stack().flex_1()
        };
        let actions_column = h_stack()
            .when(search.replace_enabled, |this| {
                this.children([
                    IconButton::new("project-search-replace-next", Icon::ReplaceNext)
                        .on_click(cx.listener(|this, _, cx| {
                            if let Some(search) = this.active_project_search.as_ref() {
                                search.update(cx, |this, cx| {
                                    this.replace_next(&ReplaceNext, cx);
                                })
                            }
                        }))
                        .tooltip(|cx| Tooltip::for_action("Replace next match", &ReplaceNext, cx)),
                    IconButton::new("project-search-replace-all", Icon::ReplaceAll)
                        .on_click(cx.listener(|this, _, cx| {
                            if let Some(search) = this.active_project_search.as_ref() {
                                search.update(cx, |this, cx| {
                                    this.replace_all(&ReplaceAll, cx);
                                })
                            }
                        }))
                        .tooltip(|cx| Tooltip::for_action("Replace all matches", &ReplaceAll, cx)),
                ])
            })
            .when_some(search.active_match_index, |mut this, index| {
                let index = index + 1;
                let match_quantity = search.model.read(cx).match_ranges.len();
                if match_quantity > 0 {
                    debug_assert!(match_quantity >= index);
                    this = this.child(Label::new(format!("{index}/{match_quantity}")))
                }
                this
            })
            .children([
                IconButton::new("project-search-prev-match", Icon::ChevronLeft)
                    .disabled(search.active_match_index.is_none())
                    .on_click(cx.listener(|this, _, cx| {
                        if let Some(search) = this.active_project_search.as_ref() {
                            search.update(cx, |this, cx| {
                                this.select_match(Direction::Prev, cx);
                            })
                        }
                    }))
                    .tooltip(|cx| {
                        Tooltip::for_action("Go to previous match", &SelectPrevMatch, cx)
                    }),
                IconButton::new("project-search-next-match", Icon::ChevronRight)
                    .disabled(search.active_match_index.is_none())
                    .on_click(cx.listener(|this, _, cx| {
                        if let Some(search) = this.active_project_search.as_ref() {
                            search.update(cx, |this, cx| {
                                this.select_match(Direction::Next, cx);
                            })
                        }
                    }))
                    .tooltip(|cx| Tooltip::for_action("Go to next match", &SelectNextMatch, cx)),
            ]);
        v_stack()
            .key_context(key_context)
            .p_1()
            .m_2()
            .gap_2()
            .justify_between()
            .on_action(cx.listener(|this, _: &ToggleFilters, cx| {
                this.toggle_filters(cx);
            }))
            .on_action(cx.listener(|this, _: &ActivateTextMode, cx| {
                this.activate_search_mode(SearchMode::Text, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateRegexMode, cx| {
                this.activate_search_mode(SearchMode::Regex, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateSemanticMode, cx| {
                this.activate_search_mode(SearchMode::Semantic, cx)
            }))
            .on_action(cx.listener(|this, action, cx| {
                this.tab(action, cx);
            }))
            .on_action(cx.listener(|this, action, cx| {
                this.tab_previous(action, cx);
            }))
            .on_action(cx.listener(|this, action, cx| {
                this.cycle_mode(action, cx);
            }))
            .when(search.current_mode != SearchMode::Semantic, |this| {
                this.on_action(cx.listener(|this, action, cx| {
                    this.toggle_replace(action, cx);
                }))
                .on_action(cx.listener(|this, _: &ToggleWholeWord, cx| {
                    this.toggle_search_option(SearchOptions::WHOLE_WORD, cx);
                }))
                .on_action(cx.listener(|this, _: &ToggleCaseSensitive, cx| {
                    this.toggle_search_option(SearchOptions::CASE_SENSITIVE, cx);
                }))
                .on_action(cx.listener(|this, action, cx| {
                    if let Some(search) = this.active_project_search.as_ref() {
                        search.update(cx, |this, cx| {
                            this.replace_next(action, cx);
                        })
                    }
                }))
                .on_action(cx.listener(|this, action, cx| {
                    if let Some(search) = this.active_project_search.as_ref() {
                        search.update(cx, |this, cx| {
                            this.replace_all(action, cx);
                        })
                    }
                }))
                .when(search.filters_enabled, |this| {
                    this.on_action(cx.listener(|this, _: &ToggleIncludeIgnored, cx| {
                        this.toggle_search_option(SearchOptions::INCLUDE_IGNORED, cx);
                    }))
                })
            })
            .child(query_column)
            .child(mode_column)
            .child(replace_column)
            .child(actions_column)
    }
}
// impl Entity for ProjectSearchBar {
//     type Event = ();
// }

// impl View for ProjectSearchBar {
//     fn ui_name() -> &'static str {
//         "ProjectSearchBar"
//     }

//     fn update_keymap_context(
//         &self,
//         keymap: &mut gpui::keymap_matcher::KeymapContext,
//         cx: &AppContext,
//     ) {
//         Self::reset_to_default_keymap_context(keymap);
//         let in_replace = self
//             .active_project_search
//             .as_ref()
//             .map(|search| {
//                 search
//                     .read(cx)
//                     .replacement_editor
//                     .read_with(cx, |_, cx| cx.is_self_focused())
//             })
//             .flatten()
//             .unwrap_or(false);
//         if in_replace {
//             keymap.add_identifier("in_replace");
//         }
//     }

//     fn render(&mut self, cx: &mut ViewContext<Self>) -> AnyElement<Self> {
//         if let Some(_search) = self.active_project_search.as_ref() {
//             let search = _search.read(cx);
//             let theme = theme::current(cx).clone();
//             let query_container_style = if search.panels_with_errors.contains(&InputPanel::Query) {
//                 theme.search.invalid_editor
//             } else {
//                 theme.search.editor.input.container
//             };

//             let search = _search.read(cx);
//             let filter_button = render_option_button_icon(
//                 search.filters_enabled,
//                 "icons/filter.svg",
//                 0,
//                 "Toggle filters",
//                 Box::new(ToggleFilters),
//                 move |_, this, cx| {
//                     this.toggle_filters(cx);
//                 },
//                 cx,
//             );

//             let search = _search.read(cx);
//             let is_semantic_available = SemanticIndex::enabled(cx);
//             let is_semantic_disabled = search.semantic_state.is_none();
//             let icon_style = theme.search.editor_icon.clone();
//             let is_active = search.active_match_index.is_some();

//             let render_option_button_icon = |path, option, cx: &mut ViewContext<Self>| {
//                 crate::search_bar::render_option_button_icon(
//                     self.is_option_enabled(option, cx),
//                     path,
//                     option.bits as usize,
//                     format!("Toggle {}", option.label()),
//                     option.to_toggle_action(),
//                     move |_, this, cx| {
//                         this.toggle_search_option(option, cx);
//                     },
//                     cx,
//                 )
//             };
//             let case_sensitive = is_semantic_disabled.then(|| {
//                 render_option_button_icon(
//                     "icons/case_insensitive.svg",
//                     SearchOptions::CASE_SENSITIVE,
//                     cx,
//                 )
//             });

//             let whole_word = is_semantic_disabled.then(|| {
//                 render_option_button_icon("icons/word_search.svg", SearchOptions::WHOLE_WORD, cx)
//             });

//             let include_ignored = is_semantic_disabled.then(|| {
//                 render_option_button_icon(
//                     "icons/file_icons/git.svg",
//                     SearchOptions::INCLUDE_IGNORED,
//                     cx,
//                 )
//             });

//             let search_button_for_mode = |mode, side, cx: &mut ViewContext<ProjectSearchBar>| {
//                 let is_active = if let Some(search) = self.active_project_search.as_ref() {
//                     let search = search.read(cx);
//                     search.current_mode == mode
//                 } else {
//                     false
//                 };
//                 render_search_mode_button(
//                     mode,
//                     side,
//                     is_active,
//                     move |_, this, cx| {
//                         this.activate_search_mode(mode, cx);
//                     },
//                     cx,
//                 )
//             };

//             let search = _search.read(cx);

//             let include_container_style =
//                 if search.panels_with_errors.contains(&InputPanel::Include) {
//                     theme.search.invalid_include_exclude_editor
//                 } else {
//                     theme.search.include_exclude_editor.input.container
//                 };

//             let exclude_container_style =
//                 if search.panels_with_errors.contains(&InputPanel::Exclude) {
//                     theme.search.invalid_include_exclude_editor
//                 } else {
//                     theme.search.include_exclude_editor.input.container
//                 };

//             let matches = search.active_match_index.map(|match_ix| {
//                 Label::new(
//                     format!(
//                         "{}/{}",
//                         match_ix + 1,
//                         search.model.read(cx).match_ranges.len()
//                     ),
//                     theme.search.match_index.text.clone(),
//                 )
//                 .contained()
//                 .with_style(theme.search.match_index.container)
//                 .aligned()
//             });
//             let should_show_replace_input = search.replace_enabled;
//             let replacement = should_show_replace_input.then(|| {
//                 Flex::row()
//                     .with_child(
//                         Svg::for_style(theme.search.replace_icon.clone().icon)
//                             .contained()
//                             .with_style(theme.search.replace_icon.clone().container),
//                     )
//                     .with_child(ChildView::new(&search.replacement_editor, cx).flex(1., true))
//                     .align_children_center()
//                     .flex(1., true)
//                     .contained()
//                     .with_style(query_container_style)
//                     .constrained()
//                     .with_min_width(theme.search.editor.min_width)
//                     .with_max_width(theme.search.editor.max_width)
//                     .with_height(theme.search.search_bar_row_height)
//                     .flex(1., false)
//             });
//             let replace_all = should_show_replace_input.then(|| {
//                 super::replace_action(
//                     ReplaceAll,
//                     "Replace all",
//                     "icons/replace_all.svg",
//                     theme.tooltip.clone(),
//                     theme.search.action_button.clone(),
//                 )
//             });
//             let replace_next = should_show_replace_input.then(|| {
//                 super::replace_action(
//                     ReplaceNext,
//                     "Replace next",
//                     "icons/replace_next.svg",
//                     theme.tooltip.clone(),
//                     theme.search.action_button.clone(),
//                 )
//             });
//             let query_column = Flex::column()
//                 .with_spacing(theme.search.search_row_spacing)
//                 .with_child(
//                     Flex::row()
//                         .with_child(
//                             Svg::for_style(icon_style.icon)
//                                 .contained()
//                                 .with_style(icon_style.container),
//                         )
//                         .with_child(ChildView::new(&search.query_editor, cx).flex(1., true))
//                         .with_child(
//                             Flex::row()
//                                 .with_child(filter_button)
//                                 .with_children(case_sensitive)
//                                 .with_children(whole_word)
//                                 .flex(1., false)
//                                 .constrained()
//                                 .contained(),
//                         )
//                         .align_children_center()
//                         .contained()
//                         .with_style(query_container_style)
//                         .constrained()
//                         .with_min_width(theme.search.editor.min_width)
//                         .with_max_width(theme.search.editor.max_width)
//                         .with_height(theme.search.search_bar_row_height)
//                         .flex(1., false),
//                 )
//                 .with_children(search.filters_enabled.then(|| {
//                     Flex::row()
//                         .with_child(
//                             Flex::row()
//                                 .with_child(
//                                     ChildView::new(&search.included_files_editor, cx)
//                                         .contained()
//                                         .constrained()
//                                         .with_height(theme.search.search_bar_row_height)
//                                         .flex(1., true),
//                                 )
//                                 .with_children(include_ignored)
//                                 .contained()
//                                 .with_style(include_container_style)
//                                 .constrained()
//                                 .with_height(theme.search.search_bar_row_height)
//                                 .flex(1., true),
//                         )
//                         .with_child(
//                             ChildView::new(&search.excluded_files_editor, cx)
//                                 .contained()
//                                 .with_style(exclude_container_style)
//                                 .constrained()
//                                 .with_height(theme.search.search_bar_row_height)
//                                 .flex(1., true),
//                         )
//                         .constrained()
//                         .with_min_width(theme.search.editor.min_width)
//                         .with_max_width(theme.search.editor.max_width)
//                         .flex(1., false)
//                 }))
//                 .flex(1., false);
//             let switches_column = Flex::row()
//                 .align_children_center()
//                 .with_child(super::toggle_replace_button(
//                     search.replace_enabled,
//                     theme.tooltip.clone(),
//                     theme.search.option_button_component.clone(),
//                 ))
//                 .constrained()
//                 .with_height(theme.search.search_bar_row_height)
//                 .contained()
//                 .with_style(theme.search.option_button_group);
//             let mode_column =
//                 Flex::row()
//                     .with_child(search_button_for_mode(
//                         SearchMode::Text,
//                         Some(Side::Left),
//                         cx,
//                     ))
//                     .with_child(search_button_for_mode(
//                         SearchMode::Regex,
//                         if is_semantic_available {
//                             None
//                         } else {
//                             Some(Side::Right)
//                         },
//                         cx,
//                     ))
//                     .with_children(is_semantic_available.then(|| {
//                         search_button_for_mode(SearchMode::Semantic, Some(Side::Right), cx)
//                     }))
//                     .contained()
//                     .with_style(theme.search.modes_container);

//             let nav_button_for_direction = |label, direction, cx: &mut ViewContext<Self>| {
//                 render_nav_button(
//                     label,
//                     direction,
//                     is_active,
//                     move |_, this, cx| {
//                         if let Some(search) = this.active_project_search.as_ref() {
//                             search.update(cx, |search, cx| search.select_match(direction, cx));
//                         }
//                     },
//                     cx,
//                 )
//             };

//             let nav_column = Flex::row()
//                 .with_children(replace_next)
//                 .with_children(replace_all)
//                 .with_child(Flex::row().with_children(matches))
//                 .with_child(nav_button_for_direction("<", Direction::Prev, cx))
//                 .with_child(nav_button_for_direction(">", Direction::Next, cx))
//                 .constrained()
//                 .with_height(theme.search.search_bar_row_height)
//                 .flex_float();

//             Flex::row()
//                 .with_child(query_column)
//                 .with_child(mode_column)
//                 .with_child(switches_column)
//                 .with_children(replacement)
//                 .with_child(nav_column)
//                 .contained()
//                 .with_style(theme.search.container)
//                 .into_any_named("project search")
//         } else {
//             Empty::new().into_any()
//         }
//     }
// }

impl EventEmitter<ToolbarItemEvent> for ProjectSearchBar {}

impl ToolbarItemView for ProjectSearchBar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        cx: &mut ViewContext<Self>,
    ) -> ToolbarItemLocation {
        cx.notify();
        self.subscription = None;
        self.active_project_search = None;
        if let Some(search) = active_pane_item.and_then(|i| i.downcast::<ProjectSearchView>()) {
            search.update(cx, |search, cx| {
                if search.current_mode == SearchMode::Semantic {
                    search.index_project(cx);
                }
            });

            self.subscription = Some(cx.observe(&search, |_, _, cx| cx.notify()));
            self.active_project_search = Some(search);
            ToolbarItemLocation::PrimaryLeft {}
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn row_count(&self, cx: &WindowContext<'_>) -> usize {
        if let Some(search) = self.active_project_search.as_ref() {
            if search.read(cx).filters_enabled {
                return 2;
            }
        }
        1
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use editor::DisplayPoint;
    use gpui::{Action, TestAppContext};
    use project::FakeFs;
    use semantic_index::semantic_index_settings::SemanticIndexSettings;
    use serde_json::json;
    use settings::{Settings, SettingsStore};

    #[gpui::test]
    async fn test_project_search(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            "/dir",
            json!({
                "one.rs": "const ONE: usize = 1;",
                "two.rs": "const TWO: usize = one::ONE + one::ONE;",
                "three.rs": "const THREE: usize = one::ONE + two::TWO;",
                "four.rs": "const FOUR: usize = one::ONE + three::THREE;",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), ["/dir".as_ref()], cx).await;
        let search = cx.build_model(|cx| ProjectSearch::new(project, cx));
        let search_view = cx.add_window(|cx| ProjectSearchView::new(search.clone(), cx, None));

        search_view
            .update(cx, |search_view, cx| {
                search_view
                    .query_editor
                    .update(cx, |query_editor, cx| query_editor.set_text("TWO", cx));
                search_view.search(cx);
            })
            .unwrap();
        cx.background_executor.run_until_parked();
        search_view.update(cx, |search_view, cx| {
            assert_eq!(
                search_view
                    .results_editor
                    .update(cx, |editor, cx| editor.display_text(cx)),
                "\n\nconst THREE: usize = one::ONE + two::TWO;\n\n\nconst TWO: usize = one::ONE + one::ONE;"
            );
            let match_background_color = cx.theme().colors().search_match_background;
            assert_eq!(
                search_view
                    .results_editor
                    .update(cx, |editor, cx| editor.all_text_background_highlights(cx)),
                &[
                    (
                        DisplayPoint::new(2, 32)..DisplayPoint::new(2, 35),
                        match_background_color
                    ),
                    (
                        DisplayPoint::new(2, 37)..DisplayPoint::new(2, 40),
                        match_background_color
                    ),
                    (
                        DisplayPoint::new(5, 6)..DisplayPoint::new(5, 9),
                        match_background_color
                    )
                ]
            );
            assert_eq!(search_view.active_match_index, Some(0));
            assert_eq!(
                search_view
                    .results_editor
                    .update(cx, |editor, cx| editor.selections.display_ranges(cx)),
                [DisplayPoint::new(2, 32)..DisplayPoint::new(2, 35)]
            );

            search_view.select_match(Direction::Next, cx);
        }).unwrap();

        search_view
            .update(cx, |search_view, cx| {
                assert_eq!(search_view.active_match_index, Some(1));
                assert_eq!(
                    search_view
                        .results_editor
                        .update(cx, |editor, cx| editor.selections.display_ranges(cx)),
                    [DisplayPoint::new(2, 37)..DisplayPoint::new(2, 40)]
                );
                search_view.select_match(Direction::Next, cx);
            })
            .unwrap();

        search_view
            .update(cx, |search_view, cx| {
                assert_eq!(search_view.active_match_index, Some(2));
                assert_eq!(
                    search_view
                        .results_editor
                        .update(cx, |editor, cx| editor.selections.display_ranges(cx)),
                    [DisplayPoint::new(5, 6)..DisplayPoint::new(5, 9)]
                );
                search_view.select_match(Direction::Next, cx);
            })
            .unwrap();

        search_view
            .update(cx, |search_view, cx| {
                assert_eq!(search_view.active_match_index, Some(0));
                assert_eq!(
                    search_view
                        .results_editor
                        .update(cx, |editor, cx| editor.selections.display_ranges(cx)),
                    [DisplayPoint::new(2, 32)..DisplayPoint::new(2, 35)]
                );
                search_view.select_match(Direction::Prev, cx);
            })
            .unwrap();

        search_view
            .update(cx, |search_view, cx| {
                assert_eq!(search_view.active_match_index, Some(2));
                assert_eq!(
                    search_view
                        .results_editor
                        .update(cx, |editor, cx| editor.selections.display_ranges(cx)),
                    [DisplayPoint::new(5, 6)..DisplayPoint::new(5, 9)]
                );
                search_view.select_match(Direction::Prev, cx);
            })
            .unwrap();

        search_view
            .update(cx, |search_view, cx| {
                assert_eq!(search_view.active_match_index, Some(1));
                assert_eq!(
                    search_view
                        .results_editor
                        .update(cx, |editor, cx| editor.selections.display_ranges(cx)),
                    [DisplayPoint::new(2, 37)..DisplayPoint::new(2, 40)]
                );
            })
            .unwrap();
    }

    #[gpui::test]
    async fn test_project_search_focus(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            "/dir",
            json!({
                "one.rs": "const ONE: usize = 1;",
                "two.rs": "const TWO: usize = one::ONE + one::ONE;",
                "three.rs": "const THREE: usize = one::ONE + two::TWO;",
                "four.rs": "const FOUR: usize = one::ONE + three::THREE;",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), ["/dir".as_ref()], cx).await;
        let window = cx.add_window(|cx| Workspace::test_new(project, cx));
        let workspace = window.clone();

        let active_item = cx.read(|cx| {
            workspace
                .read(cx)
                .unwrap()
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<ProjectSearchView>())
        });
        assert!(
            active_item.is_none(),
            "Expected no search panel to be active"
        );

        workspace
            .update(cx, |workspace, cx| {
                ProjectSearchView::deploy(workspace, &workspace::NewSearch, cx)
            })
            .unwrap();

        let Some(search_view) = cx.read(|cx| {
            workspace
                .read(cx)
                .unwrap()
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<ProjectSearchView>())
        }) else {
            panic!("Search view expected to appear after new search event trigger")
        };

        cx.spawn(|mut cx| async move {
            window
                .update(&mut cx, |_, cx| {
                    cx.dispatch_action(ToggleFocus.boxed_clone())
                })
                .unwrap();
        })
        .detach();
        cx.background_executor.run_until_parked();

        window.update(cx, |_, cx| {
            search_view.update(cx, |search_view, cx| {
                    assert!(
                        search_view.query_editor.focus_handle(cx).is_focused(cx),
                        "Empty search view should be focused after the toggle focus event: no results panel to focus on",
                    );
                });
        }).unwrap();

        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    let query_editor = &search_view.query_editor;
                    assert!(
                        query_editor.focus_handle(cx).is_focused(cx),
                        "Search view should be focused after the new search view is activated",
                    );
                    let query_text = query_editor.read(cx).text(cx);
                    assert!(
                        query_text.is_empty(),
                        "New search query should be empty but got '{query_text}'",
                    );
                    let results_text = search_view
                        .results_editor
                        .update(cx, |editor, cx| editor.display_text(cx));
                    assert!(
                        results_text.is_empty(),
                        "Empty search view should have no results but got '{results_text}'"
                    );
                });
            })
            .unwrap();

        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    search_view.query_editor.update(cx, |query_editor, cx| {
                        query_editor.set_text("sOMETHINGtHATsURELYdOESnOTeXIST", cx)
                    });
                    search_view.search(cx);
                });
            })
            .unwrap();

        cx.background_executor.run_until_parked();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    let results_text = search_view
                        .results_editor
                        .update(cx, |editor, cx| editor.display_text(cx));
                    assert!(
                results_text.is_empty(),
                "Search view for mismatching query should have no results but got '{results_text}'"
            );
                    assert!(
                search_view.query_editor.focus_handle(cx).is_focused(cx),
                "Search view should be focused after mismatching query had been used in search",
            );
                });
            })
            .unwrap();
        cx.spawn(|mut cx| async move {
            window.update(&mut cx, |_, cx| {
                cx.dispatch_action(ToggleFocus.boxed_clone())
            })
        })
        .detach();
        cx.background_executor.run_until_parked();
        window.update(cx, |_, cx| {
            search_view.update(cx, |search_view, cx| {
                    assert!(
                        search_view.query_editor.focus_handle(cx).is_focused(cx),
                        "Search view with mismatching query should be focused after the toggle focus event: still no results panel to focus on",
                    );
                });
        }).unwrap();

        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    search_view
                        .query_editor
                        .update(cx, |query_editor, cx| query_editor.set_text("TWO", cx));
                    search_view.search(cx);
                })
            })
            .unwrap();
        cx.background_executor.run_until_parked();
        window.update(cx, |_, cx|
        search_view.update(cx, |search_view, cx| {
                assert_eq!(
                    search_view
                        .results_editor
                        .update(cx, |editor, cx| editor.display_text(cx)),
                    "\n\nconst THREE: usize = one::ONE + two::TWO;\n\n\nconst TWO: usize = one::ONE + one::ONE;",
                    "Search view results should match the query"
                );
                assert!(
                    search_view.results_editor.focus_handle(cx).is_focused(cx),
                    "Search view with mismatching query should be focused after search results are available",
                );
            })).unwrap();
        cx.spawn(|mut cx| async move {
            window
                .update(&mut cx, |_, cx| {
                    cx.dispatch_action(ToggleFocus.boxed_clone())
                })
                .unwrap();
        })
        .detach();
        cx.background_executor.run_until_parked();
        window.update(cx, |_, cx| {
            search_view.update(cx, |search_view, cx| {
                    assert!(
                        search_view.results_editor.focus_handle(cx).is_focused(cx),
                        "Search view with matching query should still have its results editor focused after the toggle focus event",
                    );
                });
        }).unwrap();

        workspace
            .update(cx, |workspace, cx| {
                ProjectSearchView::deploy(workspace, &workspace::NewSearch, cx)
            })
            .unwrap();
        cx.background_executor.run_until_parked();
        let Some(search_view_2) = cx.read(|cx| {
            workspace
                .read(cx)
                .unwrap()
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<ProjectSearchView>())
        }) else {
            panic!("Search view expected to appear after new search event trigger")
        };
        assert!(
            search_view_2 != search_view,
            "New search view should be open after `workspace::NewSearch` event"
        );

        window.update(cx, |_, cx| {
            search_view.update(cx, |search_view, cx| {
                    assert_eq!(search_view.query_editor.read(cx).text(cx), "TWO", "First search view should not have an updated query");
                    assert_eq!(
                        search_view
                            .results_editor
                            .update(cx, |editor, cx| editor.display_text(cx)),
                        "\n\nconst THREE: usize = one::ONE + two::TWO;\n\n\nconst TWO: usize = one::ONE + one::ONE;",
                        "Results of the first search view should not update too"
                    );
                    assert!(
                        !search_view.query_editor.focus_handle(cx).is_focused(cx),
                        "Focus should be moved away from the first search view"
                    );
                });
        }).unwrap();

        window.update(cx, |_, cx| {
            search_view_2.update(cx, |search_view_2, cx| {
                    assert_eq!(
                        search_view_2.query_editor.read(cx).text(cx),
                        "two",
                        "New search view should get the query from the text cursor was at during the event spawn (first search view's first result)"
                    );
                    assert_eq!(
                        search_view_2
                            .results_editor
                            .update(cx, |editor, cx| editor.display_text(cx)),
                        "",
                        "No search results should be in the 2nd view yet, as we did not spawn a search for it"
                    );
                    assert!(
                        search_view_2.query_editor.focus_handle(cx).is_focused(cx),
                        "Focus should be moved into query editor fo the new window"
                    );
                });
        }).unwrap();

        window
            .update(cx, |_, cx| {
                search_view_2.update(cx, |search_view_2, cx| {
                    search_view_2
                        .query_editor
                        .update(cx, |query_editor, cx| query_editor.set_text("FOUR", cx));
                    search_view_2.search(cx);
                });
            })
            .unwrap();

        cx.background_executor.run_until_parked();
        window.update(cx, |_, cx| {
            search_view_2.update(cx, |search_view_2, cx| {
                    assert_eq!(
                        search_view_2
                            .results_editor
                            .update(cx, |editor, cx| editor.display_text(cx)),
                        "\n\nconst FOUR: usize = one::ONE + three::THREE;",
                        "New search view with the updated query should have new search results"
                    );
                    assert!(
                        search_view_2.results_editor.focus_handle(cx).is_focused(cx),
                        "Search view with mismatching query should be focused after search results are available",
                    );
                });
        }).unwrap();

        cx.spawn(|mut cx| async move {
            window
                .update(&mut cx, |_, cx| {
                    cx.dispatch_action(ToggleFocus.boxed_clone())
                })
                .unwrap();
        })
        .detach();
        cx.background_executor.run_until_parked();
        window.update(cx, |_, cx| {
            search_view_2.update(cx, |search_view_2, cx| {
                    assert!(
                        search_view_2.results_editor.focus_handle(cx).is_focused(cx),
                        "Search view with matching query should switch focus to the results editor after the toggle focus event",
                    );
                });}).unwrap();
    }

    #[gpui::test]
    async fn test_new_project_search_in_directory(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            "/dir",
            json!({
                "a": {
                    "one.rs": "const ONE: usize = 1;",
                    "two.rs": "const TWO: usize = one::ONE + one::ONE;",
                },
                "b": {
                    "three.rs": "const THREE: usize = one::ONE + two::TWO;",
                    "four.rs": "const FOUR: usize = one::ONE + three::THREE;",
                },
            }),
        )
        .await;
        let project = Project::test(fs.clone(), ["/dir".as_ref()], cx).await;
        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees().next().unwrap().read(cx).id()
        });
        let window = cx.add_window(|cx| Workspace::test_new(project, cx));
        let workspace = window.root(cx).unwrap();

        let active_item = cx.read(|cx| {
            workspace
                .read(cx)
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<ProjectSearchView>())
        });
        assert!(
            active_item.is_none(),
            "Expected no search panel to be active"
        );

        let one_file_entry = cx.update(|cx| {
            workspace
                .read(cx)
                .project()
                .read(cx)
                .entry_for_path(&(worktree_id, "a/one.rs").into(), cx)
                .expect("no entry for /a/one.rs file")
        });
        assert!(one_file_entry.is_file());
        window
            .update(cx, |workspace, cx| {
                ProjectSearchView::new_search_in_directory(workspace, &one_file_entry, cx)
            })
            .unwrap();
        let active_search_entry = cx.read(|cx| {
            workspace
                .read(cx)
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<ProjectSearchView>())
        });
        assert!(
            active_search_entry.is_none(),
            "Expected no search panel to be active for file entry"
        );

        let a_dir_entry = cx.update(|cx| {
            workspace
                .read(cx)
                .project()
                .read(cx)
                .entry_for_path(&(worktree_id, "a").into(), cx)
                .expect("no entry for /a/ directory")
        });
        assert!(a_dir_entry.is_dir());
        window
            .update(cx, |workspace, cx| {
                ProjectSearchView::new_search_in_directory(workspace, &a_dir_entry, cx)
            })
            .unwrap();

        let Some(search_view) = cx.read(|cx| {
            workspace
                .read(cx)
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<ProjectSearchView>())
        }) else {
            panic!("Search view expected to appear after new search in directory event trigger")
        };
        cx.background_executor.run_until_parked();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    assert!(
                        search_view.query_editor.focus_handle(cx).is_focused(cx),
                        "On new search in directory, focus should be moved into query editor"
                    );
                    search_view.excluded_files_editor.update(cx, |editor, cx| {
                        assert!(
                            editor.display_text(cx).is_empty(),
                            "New search in directory should not have any excluded files"
                        );
                    });
                    search_view.included_files_editor.update(cx, |editor, cx| {
                        assert_eq!(
                            editor.display_text(cx),
                            a_dir_entry.path.to_str().unwrap(),
                            "New search in directory should have included dir entry path"
                        );
                    });
                });
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    search_view
                        .query_editor
                        .update(cx, |query_editor, cx| query_editor.set_text("const", cx));
                    search_view.search(cx);
                });
            })
            .unwrap();
        cx.background_executor.run_until_parked();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    assert_eq!(
                search_view
                    .results_editor
                    .update(cx, |editor, cx| editor.display_text(cx)),
                "\n\nconst ONE: usize = 1;\n\n\nconst TWO: usize = one::ONE + one::ONE;",
                "New search in directory should have a filter that matches a certain directory"
            );
                })
            })
            .unwrap();
    }

    #[gpui::test]
    async fn test_search_query_history(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            "/dir",
            json!({
                "one.rs": "const ONE: usize = 1;",
                "two.rs": "const TWO: usize = one::ONE + one::ONE;",
                "three.rs": "const THREE: usize = one::ONE + two::TWO;",
                "four.rs": "const FOUR: usize = one::ONE + three::THREE;",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), ["/dir".as_ref()], cx).await;
        let window = cx.add_window(|cx| Workspace::test_new(project, cx));
        let workspace = window.root(cx).unwrap();
        window
            .update(cx, |workspace, cx| {
                ProjectSearchView::deploy(workspace, &workspace::NewSearch, cx)
            })
            .unwrap();

        let search_view = cx.read(|cx| {
            workspace
                .read(cx)
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<ProjectSearchView>())
                .expect("Search view expected to appear after new search event trigger")
        });

        let search_bar = window.build_view(cx, |cx| {
            let mut search_bar = ProjectSearchBar::new();
            search_bar.set_active_pane_item(Some(&search_view), cx);
            // search_bar.show(cx);
            search_bar
        });

        // Add 3 search items into the history + another unsubmitted one.
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    search_view.search_options = SearchOptions::CASE_SENSITIVE;
                    search_view
                        .query_editor
                        .update(cx, |query_editor, cx| query_editor.set_text("ONE", cx));
                    search_view.search(cx);
                });
            })
            .unwrap();

        cx.background_executor.run_until_parked();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    search_view
                        .query_editor
                        .update(cx, |query_editor, cx| query_editor.set_text("TWO", cx));
                    search_view.search(cx);
                });
            })
            .unwrap();
        cx.background_executor.run_until_parked();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    search_view
                        .query_editor
                        .update(cx, |query_editor, cx| query_editor.set_text("THREE", cx));
                    search_view.search(cx);
                })
            })
            .unwrap();
        cx.background_executor.run_until_parked();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    search_view.query_editor.update(cx, |query_editor, cx| {
                        query_editor.set_text("JUST_TEXT_INPUT", cx)
                    });
                })
            })
            .unwrap();
        cx.background_executor.run_until_parked();

        // Ensure that the latest input with search settings is active.
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    assert_eq!(
                        search_view.query_editor.read(cx).text(cx),
                        "JUST_TEXT_INPUT"
                    );
                    assert_eq!(search_view.search_options, SearchOptions::CASE_SENSITIVE);
                });
            })
            .unwrap();

        // Next history query after the latest should set the query to the empty string.
        window
            .update(cx, |_, cx| {
                search_bar.update(cx, |search_bar, cx| {
                    search_bar.next_history_query(&NextHistoryQuery, cx);
                })
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    assert_eq!(search_view.query_editor.read(cx).text(cx), "");
                    assert_eq!(search_view.search_options, SearchOptions::CASE_SENSITIVE);
                });
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_bar.update(cx, |search_bar, cx| {
                    search_bar.next_history_query(&NextHistoryQuery, cx);
                })
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    assert_eq!(search_view.query_editor.read(cx).text(cx), "");
                    assert_eq!(search_view.search_options, SearchOptions::CASE_SENSITIVE);
                });
            })
            .unwrap();

        // First previous query for empty current query should set the query to the latest submitted one.
        window
            .update(cx, |_, cx| {
                search_bar.update(cx, |search_bar, cx| {
                    search_bar.previous_history_query(&PreviousHistoryQuery, cx);
                });
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    assert_eq!(search_view.query_editor.read(cx).text(cx), "THREE");
                    assert_eq!(search_view.search_options, SearchOptions::CASE_SENSITIVE);
                });
            })
            .unwrap();

        // Further previous items should go over the history in reverse order.
        window
            .update(cx, |_, cx| {
                search_bar.update(cx, |search_bar, cx| {
                    search_bar.previous_history_query(&PreviousHistoryQuery, cx);
                });
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    assert_eq!(search_view.query_editor.read(cx).text(cx), "TWO");
                    assert_eq!(search_view.search_options, SearchOptions::CASE_SENSITIVE);
                });
            })
            .unwrap();

        // Previous items should never go behind the first history item.
        window
            .update(cx, |_, cx| {
                search_bar.update(cx, |search_bar, cx| {
                    search_bar.previous_history_query(&PreviousHistoryQuery, cx);
                });
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    assert_eq!(search_view.query_editor.read(cx).text(cx), "ONE");
                    assert_eq!(search_view.search_options, SearchOptions::CASE_SENSITIVE);
                });
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_bar.update(cx, |search_bar, cx| {
                    search_bar.previous_history_query(&PreviousHistoryQuery, cx);
                });
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    assert_eq!(search_view.query_editor.read(cx).text(cx), "ONE");
                    assert_eq!(search_view.search_options, SearchOptions::CASE_SENSITIVE);
                });
            })
            .unwrap();

        // Next items should go over the history in the original order.
        window
            .update(cx, |_, cx| {
                search_bar.update(cx, |search_bar, cx| {
                    search_bar.next_history_query(&NextHistoryQuery, cx);
                });
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    assert_eq!(search_view.query_editor.read(cx).text(cx), "TWO");
                    assert_eq!(search_view.search_options, SearchOptions::CASE_SENSITIVE);
                });
            })
            .unwrap();

        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    search_view
                        .query_editor
                        .update(cx, |query_editor, cx| query_editor.set_text("TWO_NEW", cx));
                    search_view.search(cx);
                });
            })
            .unwrap();
        cx.background_executor.run_until_parked();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    assert_eq!(search_view.query_editor.read(cx).text(cx), "TWO_NEW");
                    assert_eq!(search_view.search_options, SearchOptions::CASE_SENSITIVE);
                });
            })
            .unwrap();

        // New search input should add another entry to history and move the selection to the end of the history.
        window
            .update(cx, |_, cx| {
                search_bar.update(cx, |search_bar, cx| {
                    search_bar.previous_history_query(&PreviousHistoryQuery, cx);
                });
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    assert_eq!(search_view.query_editor.read(cx).text(cx), "THREE");
                    assert_eq!(search_view.search_options, SearchOptions::CASE_SENSITIVE);
                });
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_bar.update(cx, |search_bar, cx| {
                    search_bar.previous_history_query(&PreviousHistoryQuery, cx);
                });
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    assert_eq!(search_view.query_editor.read(cx).text(cx), "TWO");
                    assert_eq!(search_view.search_options, SearchOptions::CASE_SENSITIVE);
                });
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_bar.update(cx, |search_bar, cx| {
                    search_bar.next_history_query(&NextHistoryQuery, cx);
                });
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    assert_eq!(search_view.query_editor.read(cx).text(cx), "THREE");
                    assert_eq!(search_view.search_options, SearchOptions::CASE_SENSITIVE);
                });
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_bar.update(cx, |search_bar, cx| {
                    search_bar.next_history_query(&NextHistoryQuery, cx);
                });
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    assert_eq!(search_view.query_editor.read(cx).text(cx), "TWO_NEW");
                    assert_eq!(search_view.search_options, SearchOptions::CASE_SENSITIVE);
                });
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_bar.update(cx, |search_bar, cx| {
                    search_bar.next_history_query(&NextHistoryQuery, cx);
                });
            })
            .unwrap();
        window
            .update(cx, |_, cx| {
                search_view.update(cx, |search_view, cx| {
                    assert_eq!(search_view.query_editor.read(cx).text(cx), "");
                    assert_eq!(search_view.search_options, SearchOptions::CASE_SENSITIVE);
                });
            })
            .unwrap();
    }

    pub fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            cx.set_global(ActiveSearches::default());
            SemanticIndexSettings::register(cx);

            theme::init(theme::LoadThemes::JustBase, cx);

            language::init(cx);
            client::init_settings(cx);
            editor::init(cx);
            workspace::init_settings(cx);
            Project::init_settings(cx);
            super::init(cx);
        });
    }
}
