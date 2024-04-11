use std::collections::{HashMap, HashSet};
use std::sync::atomic::{self, AtomicUsize};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use arc_swap::ArcSwap;
use futures_util::stream::{FusedStream, FuturesUnordered};
use futures_util::{Future, StreamExt};
use helix_core::syntax::LanguageServerFeature;
use helix_event::{cancelable_future, cancelation, CancelRx, CancelTx};
use helix_lsp::lsp::{CompletionContext, CompletionTriggerKind};
use helix_lsp::util::pos_to_lsp_pos;
use helix_lsp::{lsp, LanguageServerId};
use helix_stdx::rope::RopeSliceExt;
use helix_view::document::Mode;
use helix_view::handlers::lsp::CompletionEvent;
use helix_view::{Document, DocumentId, Editor, ViewId};
use tokio::pin;
use tokio::time::{timeout_at, Instant};

use crate::compositor::Compositor;
use crate::config::Config;
use crate::handlers::completion::{replace_completions, show_completion, CompletionItem};
use crate::job::{dispatch, dispatch_blocking};
use crate::ui;
use crate::ui::editor::InsertEvent;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum TriggerKind {
    Auto,
    TriggerChar,
    Manual,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Trigger {
    pub(super) pos: usize,
    pub(super) view: ViewId,
    pub(super) doc: DocumentId,
    pub(super) kind: TriggerKind,
}

#[derive(Debug)]
pub struct CompletionHandler {
    /// currently active trigger which will cause a
    /// completion request after the timeout
    trigger: Option<Trigger>,
    /// A handle for currently active completion request.
    /// This can be used to determine whether the current
    /// request is still active (and new triggers should be
    /// ignored) and can also be used to abort the current
    /// request (by dropping the handle)
    request: Option<CancelTx>,
    config: Arc<ArcSwap<Config>>,
}

impl CompletionHandler {
    pub fn new(config: Arc<ArcSwap<Config>>) -> CompletionHandler {
        Self {
            config,
            request: None,
            trigger: None,
        }
    }
}

impl helix_event::AsyncHook for CompletionHandler {
    type Event = CompletionEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        _old_timeout: Option<Instant>,
    ) -> Option<Instant> {
        match event {
            CompletionEvent::AutoTrigger {
                cursor: trigger_pos,
                doc,
                view,
            } => {
                // techically it shouldn't be possible to switch views/documents in insert mode
                // but people may create weird keymaps/use the mouse so lets be extra careful
                if self
                    .trigger
                    .as_ref()
                    .map_or(true, |trigger| trigger.doc != doc || trigger.view != view)
                {
                    self.trigger = Some(Trigger {
                        pos: trigger_pos,
                        view,
                        doc,
                        kind: TriggerKind::Auto,
                    });
                }
            }
            CompletionEvent::TriggerChar { cursor, doc, view } => {
                // immediately request completions and drop all auto completion requests
                self.request = None;
                self.trigger = Some(Trigger {
                    pos: cursor,
                    view,
                    doc,
                    kind: TriggerKind::TriggerChar,
                });
            }
            CompletionEvent::ManualTrigger { cursor, doc, view } => {
                // immediately request completions and drop all auto completion requests
                self.request = None;
                self.trigger = Some(Trigger {
                    pos: cursor,
                    view,
                    doc,
                    kind: TriggerKind::Manual,
                });
                // stop debouncing immediately and request the completion
                self.finish_debounce();
                return None;
            }
            CompletionEvent::Cancel => {
                self.trigger = None;
                self.request = None;
            }
            CompletionEvent::DeleteText { cursor } => {
                // if we deleted the original trigger, abort the completion
                if matches!(self.trigger, Some(Trigger{ pos, .. }) if cursor < pos) {
                    self.trigger = None;
                    self.request = None;
                }
            }
        }
        self.trigger.map(|trigger| {
            // if the current request was closed forget about it
            // otherwise immediately restart the completion request
            let cancel = self.request.take().map_or(false, |req| !req.is_closed());
            let timeout = if trigger.kind == TriggerKind::Auto && !cancel {
                self.config.load().editor.completion_timeout
            } else {
                // we want almost instant completions for trigger chars
                // and restarting completion requests. The small timeout here mainly
                // serves to better handle cases where the completion handler
                // may fall behind (so multiple events in the channel) and macros
                Duration::from_millis(5)
            };
            Instant::now() + timeout
        })
    }

    fn finish_debounce(&mut self) {
        let trigger = self.trigger.take().expect("debounce always has a trigger");
        let (tx, rx) = cancelation();
        self.request = Some(tx);
        dispatch_blocking(move |editor, compositor| {
            request_completions(trigger, rx, editor, compositor)
        });
    }
}

fn request_completions(
    mut trigger: Trigger,
    cancel: CancelRx,
    editor: &mut Editor,
    compositor: &mut Compositor,
) {
    let (view, doc) = current!(editor);

    if compositor
        .find::<ui::EditorView>()
        .unwrap()
        .completion
        .is_some()
        || editor.mode != Mode::Insert
    {
        return;
    }

    let text = doc.text();
    let cursor = doc.selection(view.id).primary().cursor(text.slice(..));
    if trigger.view != view.id || trigger.doc != doc.id() || cursor < trigger.pos {
        return;
    }
    // this looks odd... Why are we not using the trigger position from
    // the `trigger` here? Won't that mean that the trigger char doesn't get
    // send to the LS if we type fast enougn? Yes that is true but it's
    // not actually a problem. The LSP will resolve the completion to the identifier
    // anyway (in fact sending the later position is necessary to get the right results
    // from LSPs that provide incomplete completion list). We rely on trigger offset
    // and primary cursor matching for multi-cursor completions so this is definitely
    // necessary from our side too.
    trigger.pos = cursor;
    let trigger_text = text.slice(..cursor);

    let mut seen_language_servers = HashSet::new();
    let language_servers: Vec<_> = doc
        .language_servers_with_feature(LanguageServerFeature::Completion)
        .filter(|ls| seen_language_servers.insert(ls.id()))
        .collect();
    let futures: FuturesUnordered<_> = language_servers
        .iter()
        .enumerate()
        .map(|(priority, ls)| {
            let context = if trigger.kind == TriggerKind::Manual {
                lsp::CompletionContext {
                    trigger_kind: lsp::CompletionTriggerKind::INVOKED,
                    trigger_character: None,
                }
            } else {
                let trigger_char =
                    ls.capabilities()
                        .completion_provider
                        .as_ref()
                        .and_then(|provider| {
                            provider
                                .trigger_characters
                                .as_deref()?
                                .iter()
                                .find(|&trigger| trigger_text.ends_with(trigger))
                        });

                if trigger_char.is_some() {
                    lsp::CompletionContext {
                        trigger_kind: lsp::CompletionTriggerKind::TRIGGER_CHARACTER,
                        trigger_character: trigger_char.cloned(),
                    }
                } else {
                    lsp::CompletionContext {
                        trigger_kind: lsp::CompletionTriggerKind::INVOKED,
                        trigger_character: None,
                    }
                }
            };

            request_completions_from_language_server(ls, doc, view.id, context, -(priority as i8))
        })
        .collect();

    let futures = futures.filter_map(|res: Result<_>| async {
        match res {
            Ok(response) if response.items.is_empty() && !response.incomplete => None,
            Ok(response) => Some(response),
            Err(err) => {
                log::debug!("completion request failed: {err:?}");
                None
            }
        }
    });

    let savepoint = doc.savepoint(view);

    let ui = compositor.find::<ui::EditorView>().unwrap();
    ui.last_insert.1.push(InsertEvent::RequestCompletion);
    let request_completions = async move {
        pin!(futures);
        let mut incomplete_completion_lists = HashMap::new();
        let Some(response) = futures.next().await else {
            return;
        };
        if response.incomplete {
            incomplete_completion_lists.insert(response.provider, response.priority);
        }
        let mut items: Vec<_> = response.into_items().collect();
        let deadline = Instant::now() + Duration::from_millis(100);
        while let Some(response) = timeout_at(deadline, futures.next()).await.ok().flatten() {
            if response.incomplete {
                incomplete_completion_lists.insert(response.provider, response.priority);
            }
            items.extend(response.into_items());
        }
        let version = Arc::new(AtomicUsize::new(0));
        dispatch(move |editor, compositor| {
            show_completion(
                editor,
                compositor,
                items,
                incomplete_completion_lists,
                trigger,
                savepoint,
            )
        })
        .await;
        if !futures.is_terminated() {
            replace_completions(version, 0, futures).await;
        }
    };
    tokio::spawn(cancelable_future(request_completions, cancel));
}

pub struct CompletionResponse {
    pub items: Vec<lsp::CompletionItem>,
    pub incomplete: bool,
    pub provider: LanguageServerId,
    pub priority: i8,
}

impl CompletionResponse {
    pub fn into_items(self) -> impl Iterator<Item = CompletionItem> {
        self.items.into_iter().map(move |item| CompletionItem {
            item,
            provider: self.provider,
            resolved: false,
            provider_priority: self.priority,
        })
    }
}

fn request_completions_from_language_server(
    ls: &helix_lsp::Client,
    doc: &Document,
    view: ViewId,
    context: lsp::CompletionContext,
    priority: i8,
) -> impl Future<Output = Result<CompletionResponse>> {
    let provider = ls.id();
    let offset_encoding = ls.offset_encoding();
    let text = doc.text();
    let cursor = doc.selection(view).primary().cursor(text.slice(..));
    let pos = pos_to_lsp_pos(text, cursor, offset_encoding);
    let doc_id = doc.identifier();

    log::error!(
        "request completion at {}",
        doc.selection(view).primary().fragment(doc.text().slice(..))
    );
    let completion_response = ls.completion(doc_id, pos, None, context).unwrap();
    async move {
        let json = completion_response.await?;
        let response: Option<lsp::CompletionResponse> = serde_json::from_value(json)?;
        let (mut items, incomplete) = match response {
            Some(lsp::CompletionResponse::Array(items)) => (items, false),
            Some(lsp::CompletionResponse::List(lsp::CompletionList {
                is_incomplete,
                items,
            })) => (items, is_incomplete),
            None => (Vec::new(), false),
        };
        items.sort_by(|item1, item2| {
            let sort_text1 = item1.sort_text.as_deref().unwrap_or(&item1.label);
            let sort_text2 = item2.sort_text.as_deref().unwrap_or(&item2.label);
            sort_text1.cmp(sort_text2)
        });
        Ok(CompletionResponse {
            items,
            incomplete,
            provider,
            priority,
        })
    }
}

pub fn request_incomplete_completion_list(
    editor: &mut Editor,
    incomplete_completion_lists: &mut HashMap<LanguageServerId, i8>,
    version: Arc<AtomicUsize>,
) {
    if incomplete_completion_lists.is_empty() {
        return;
    }
    let (view, doc) = current_ref!(editor);
    let futures = FuturesUnordered::new();
    incomplete_completion_lists.retain(|&id, &mut priority| {
        let Some(ls) = editor.language_server_by_id(id) else {
            return false;
        };
        let request = request_completions_from_language_server(
            ls,
            doc,
            view.id,
            CompletionContext {
                trigger_kind: CompletionTriggerKind::TRIGGER_FOR_INCOMPLETE_COMPLETIONS,
                trigger_character: None,
            },
            priority,
        );
        futures.push(request);
        true
    });
    let futures = futures.filter_map(|res: Result<_>| async {
        match res {
            Ok(response) => Some(response),
            Err(err) => {
                log::debug!("completion request failed: {err:?}");
                None
            }
        }
    });
    let initial_version = version.load(atomic::Ordering::Relaxed);
    log::error!("requestion incomplete list {initial_version}");
    tokio::spawn(async move {
        pin!(futures);
        replace_completions(version, initial_version, futures).await;
    });
}
