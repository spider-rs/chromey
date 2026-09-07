use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::map::Entry;

use chromiumoxide_cdp::cdp::browser_protocol::network::LoaderId;
use chromiumoxide_cdp::cdp::browser_protocol::page::{
    AddScriptToEvaluateOnNewDocumentParams, CreateIsolatedWorldParams, EventFrameDetached,
    EventFrameStartedLoading, EventFrameStoppedLoading, EventLifecycleEvent,
    EventNavigatedWithinDocument, Frame as CdpFrame, FrameTree,
};
use chromiumoxide_cdp::cdp::browser_protocol::target::EventAttachedToTarget;
use chromiumoxide_cdp::cdp::js_protocol::runtime::*;
use chromiumoxide_cdp::cdp::{
    browser_protocol::page::{self, FrameId},
    // js_protocol::runtime,
};
use chromiumoxide_types::{Method, MethodId, Request};
use spider_fingerprint::BASE_CHROME_VERSION;

use crate::error::DeadlineExceeded;
use crate::handler::domworld::DOMWorld;
use crate::handler::http::HttpRequest;

use crate::{cmd::CommandChain, ArcHttpRequest};

lazy_static::lazy_static! {
    /// Spoof the runtime.
    static ref EVALUATION_SCRIPT_URL: String = format!("____{}___evaluation_script__", random_world_name(&BASE_CHROME_VERSION.to_string()));
}

/// Generate a collision-resistant world name using `id` + randomness.
pub fn random_world_name(id: &str) -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    let rand_len = rng.random_range(6..=12);

    // Convert first few chars of id into base36-compatible chars
    let id_part: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(5)
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphabetic() {
                c
            } else {
                // convert 0-9 into a base36 letter offset to obscure it a bit
                (b'a' + (c as u8 - b'0') % 26) as char
            }
        })
        .collect();

    // Generate random base36 tail
    let rand_part: String = (0..rand_len)
        .filter_map(|_| std::char::from_digit(rng.random_range(0..36), 36))
        .collect();

    // Ensure first char is always a letter (10–35 => a–z)
    let first = std::char::from_digit(rng.random_range(10..36), 36).unwrap_or('a');

    format!("{first}{id_part}{rand_part}")
}

/// Represents a frame on the page
#[derive(Debug)]
pub struct Frame {
    /// The parent frame ID.
    parent_frame: Option<FrameId>,
    /// Cdp identifier of this frame
    id: FrameId,
    /// The main world.
    main_world: DOMWorld,
    /// The secondary world.
    secondary_world: DOMWorld,
    loader_id: Option<LoaderId>,
    /// Current url of this frame
    url: Option<String>,
    /// The http request that loaded this with this frame
    http_request: ArcHttpRequest,
    /// The frames contained in this frame
    child_frames: HashSet<FrameId>,
    name: Option<String>,
    /// The received lifecycle events
    lifecycle_events: HashSet<MethodId>,
    /// The isolated world name.
    isolated_world_name: String,
}

impl Frame {
    pub fn new(id: FrameId) -> Self {
        let isolated_world_name = random_world_name(id.inner());

        Self {
            parent_frame: None,
            id,
            main_world: Default::default(),
            secondary_world: Default::default(),
            loader_id: None,
            url: None,
            http_request: None,
            child_frames: Default::default(),
            name: None,
            lifecycle_events: Default::default(),
            isolated_world_name,
        }
    }

    pub fn with_parent(id: FrameId, parent: &mut Frame) -> Self {
        parent.child_frames.insert(id.clone());
        Self {
            parent_frame: Some(parent.id.clone()),
            id,
            main_world: Default::default(),
            secondary_world: Default::default(),
            loader_id: None,
            url: None,
            http_request: None,
            child_frames: Default::default(),
            name: None,
            lifecycle_events: Default::default(),
            isolated_world_name: parent.isolated_world_name.clone(),
        }
    }

    pub fn get_isolated_world_name(&self) -> &String {
        &self.isolated_world_name
    }

    pub fn parent_id(&self) -> Option<&FrameId> {
        self.parent_frame.as_ref()
    }

    pub fn id(&self) -> &FrameId {
        &self.id
    }

    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn main_world(&self) -> &DOMWorld {
        &self.main_world
    }

    pub fn secondary_world(&self) -> &DOMWorld {
        &self.secondary_world
    }

    pub fn lifecycle_events(&self) -> &HashSet<MethodId> {
        &self.lifecycle_events
    }

    pub fn http_request(&self) -> Option<&Arc<HttpRequest>> {
        self.http_request.as_ref()
    }

    fn navigated(&mut self, frame: &CdpFrame) {
        self.name.clone_from(&frame.name);
        let url = if let Some(ref fragment) = frame.url_fragment {
            format!("{}{fragment}", frame.url)
        } else {
            frame.url.clone()
        };
        self.url = Some(url);
    }

    fn navigated_within_url(&mut self, url: String) {
        self.url = Some(url)
    }

    fn on_loading_stopped(&mut self) {
        self.lifecycle_events.insert("DOMContentLoaded".into());
        self.lifecycle_events.insert("load".into());
    }

    fn on_loading_started(&mut self) {
        self.lifecycle_events.clear();
        self.http_request.take();
    }

    pub fn is_loaded(&self) -> bool {
        self.lifecycle_events.contains("load")
    }

    /// The `DOMContentLoaded` lifecycle event has fired (HTML parsed, sync
    /// scripts executed). This fires *before* `load` — subresources like
    /// images and fonts may still be in-flight.
    pub fn is_dom_content_loaded(&self) -> bool {
        self.lifecycle_events.contains("DOMContentLoaded")
    }

    /// Main frame + child frames have fired the `networkIdle` lifecycle event.
    pub fn is_network_idle(&self) -> bool {
        self.lifecycle_events.contains("networkIdle")
    }

    /// Main frame + child frames have fired the `networkAlmostIdle` lifecycle event.
    pub fn is_network_almost_idle(&self) -> bool {
        self.lifecycle_events.contains("networkAlmostIdle")
    }

    pub fn clear_contexts(&mut self) {
        self.main_world.take_context();
        self.secondary_world.take_context();
    }

    pub fn destroy_context(&mut self, ctx_unique_id: &str) {
        if self.main_world.execution_context_unique_id() == Some(ctx_unique_id) {
            self.main_world.take_context();
        } else if self.secondary_world.execution_context_unique_id() == Some(ctx_unique_id) {
            self.secondary_world.take_context();
        }
    }

    pub fn execution_context(&self) -> Option<ExecutionContextId> {
        self.main_world.execution_context()
    }

    pub fn set_request(&mut self, request: HttpRequest) {
        self.http_request = Some(Arc::new(request))
    }
}

/// Maintains the state of the pages frame and listens to events produced by
/// chromium targeting the `Target`. Also listens for events that indicate that
/// a navigation was completed
#[derive(Debug)]
pub struct FrameManager {
    main_frame: Option<FrameId>,
    frames: HashMap<FrameId, Frame>,
    /// The contexts mapped with their frames
    context_ids: HashMap<String, FrameId>,
    isolated_worlds: HashSet<String>,
    /// Timeout after which an anticipated event (related to navigation) doesn't
    /// arrive results in an error
    request_timeout: Duration,
    /// Track currently in progress navigation
    pending_navigations: VecDeque<(FrameRequestedNavigation, NavigationWatcher)>,
    /// The currently ongoing navigation
    navigation: Option<(NavigationWatcher, Instant)>,
    /// Optional cap on main-frame cross-document navigations per `goto`.
    ///
    /// Defends against JS/meta-refresh loops that keep issuing fresh top-level
    /// navigations (which look like new documents, not HTTP redirects). `None`
    /// disables the guard — preserves prior behavior. `Some(n)` aborts the
    /// in-flight navigation with `NavigationError::TooManyNavigations` once the
    /// main frame has navigated more than `n` times since the latest `goto`.
    max_main_frame_navigations: Option<u32>,
    /// Count of main-frame cross-document navigations since the last `goto`.
    main_frame_nav_count: u32,
}

impl FrameManager {
    pub fn new(request_timeout: Duration) -> Self {
        FrameManager {
            main_frame: None,
            frames: Default::default(),
            context_ids: Default::default(),
            isolated_worlds: Default::default(),
            request_timeout,
            pending_navigations: Default::default(),
            navigation: None,
            max_main_frame_navigations: None,
            main_frame_nav_count: 0,
        }
    }

    /// Set the cap on main-frame cross-document navigations per `goto`.
    /// `None` disables the guard.
    pub fn set_max_main_frame_navigations(&mut self, cap: Option<u32>) {
        self.max_main_frame_navigations = cap;
    }

    /// The commands to execute in order to initialize this frame manager
    pub fn init_commands(timeout: Duration) -> CommandChain {
        let enable = page::EnableParams::default();
        let get_tree = page::GetFrameTreeParams::default();
        let set_lifecycle = page::SetLifecycleEventsEnabledParams::new(true);
        // let enable_runtime = EnableParams::default();
        // let disable_runtime = DisableParams::default();

        let mut commands = Vec::with_capacity(3);

        let enable_id = enable.identifier();
        let get_tree_id = get_tree.identifier();
        let set_lifecycle_id = set_lifecycle.identifier();
        // let enable_runtime_id = enable_runtime.identifier();
        // let disable_runtime_id = disable_runtime.identifier();

        if let Ok(value) = serde_json::to_value(enable) {
            commands.push((enable_id, value));
        }

        if let Ok(value) = serde_json::to_value(get_tree) {
            commands.push((get_tree_id, value));
        }

        if let Ok(value) = serde_json::to_value(set_lifecycle) {
            commands.push((set_lifecycle_id, value));
        }

        // if let Ok(value) = serde_json::to_value(enable_runtime) {
        //     commands.push((enable_runtime_id, value));
        // }

        // if let Ok(value) = serde_json::to_value(disable_runtime) {
        //     commands.push((disable_runtime_id, value));
        // }

        CommandChain::new(commands, timeout)
    }

    pub fn main_frame(&self) -> Option<&Frame> {
        self.main_frame.as_ref().and_then(|id| self.frames.get(id))
    }

    pub fn main_frame_mut(&mut self) -> Option<&mut Frame> {
        if let Some(id) = self.main_frame.as_ref() {
            self.frames.get_mut(id)
        } else {
            None
        }
    }

    /// Get the main isolated world name.
    pub fn get_isolated_world_name(&self) -> Option<&String> {
        self.main_frame
            .as_ref()
            .and_then(|id| self.frames.get(id).map(|fid| fid.get_isolated_world_name()))
    }

    pub fn frames(&self) -> impl Iterator<Item = &Frame> + '_ {
        self.frames.values()
    }

    pub fn frame(&self, id: &FrameId) -> Option<&Frame> {
        self.frames.get(id)
    }

    fn check_lifecycle(&self, watcher: &NavigationWatcher, frame: &Frame) -> bool {
        watcher.expected_lifecycle.iter().all(|ev| {
            frame.lifecycle_events.contains(ev)
                || (frame.url.is_none() && frame.lifecycle_events.contains("DOMContentLoaded"))
        })
    }

    fn check_lifecycle_complete(
        &self,
        watcher: &NavigationWatcher,
        frame: &Frame,
    ) -> Option<NavigationOk> {
        if !self.check_lifecycle(watcher, frame) {
            return None;
        }
        if frame.loader_id == watcher.loader_id && !watcher.same_document_navigation {
            return None;
        }
        if watcher.same_document_navigation {
            return Some(NavigationOk::SameDocumentNavigation(watcher.id));
        }
        if frame.loader_id != watcher.loader_id {
            return Some(NavigationOk::NewDocumentNavigation(watcher.id));
        }
        None
    }

    /// Track the request in the frame
    pub fn on_http_request_finished(&mut self, request: HttpRequest) {
        if let Some(id) = request.frame.as_ref() {
            if let Some(frame) = self.frames.get_mut(id) {
                frame.set_request(request);
            }
        }
    }

    /// Drop a failed navigation so it cannot time out or hold up the next goto.
    pub fn abandon_navigation(&mut self, id: NavigationId) {
        if self
            .navigation
            .as_ref()
            .is_some_and(|(nav, _)| nav.id == id)
        {
            self.navigation = None;
        }
        // The retain is defensive rather than exercised: under `Handler::run()`
        // the navigation is always promoted into `self.navigation` before the ack
        // lands. A driver that submits differently would leave it queued, and a
        // stale queued navigation would block the next goto's promotion.
        self.pending_navigations.retain(|(req, _)| req.id != id);
    }

    pub fn poll(&mut self, now: Instant) -> Option<FrameEvent> {
        // check if the navigation completed
        if let Some((watcher, deadline)) = self.navigation.take() {
            // Navigation-loop guard: abort if the main frame has navigated
            // more than the configured cap since this `goto` started.
            if let Some(cap) = self.max_main_frame_navigations {
                if self.main_frame_nav_count > cap {
                    let count = self.main_frame_nav_count;
                    // Keep the counter positive so the next goto can still
                    // reset it cleanly; we just clear the active navigation.
                    return Some(FrameEvent::NavigationResult(Err(
                        NavigationError::TooManyNavigations {
                            id: watcher.id,
                            count,
                        },
                    )));
                }
            }

            if now > deadline {
                // navigation request timed out
                return Some(FrameEvent::NavigationResult(Err(
                    NavigationError::Timeout {
                        err: DeadlineExceeded::new(now, deadline),
                        id: watcher.id,
                    },
                )));
            }

            if let Some(frame) = self.frames.get(&watcher.frame_id) {
                if let Some(nav) = self.check_lifecycle_complete(&watcher, frame) {
                    // request is complete if the frame's lifecycle is complete = frame received all
                    // required events
                    return Some(FrameEvent::NavigationResult(Ok(nav)));
                } else {
                    // not finished yet
                    self.navigation = Some((watcher, deadline));
                }
            } else {
                return Some(FrameEvent::NavigationResult(Err(
                    NavigationError::FrameNotFound {
                        frame: watcher.frame_id,
                        id: watcher.id,
                    },
                )));
            }
        } else if let Some((req, watcher)) = self.pending_navigations.pop_front() {
            // queue in the next navigation that is must be fulfilled until `deadline`
            let deadline = Instant::now() + req.timeout;
            self.navigation = Some((watcher, deadline));
            return Some(FrameEvent::NavigationRequest(req.id, req.req));
        }
        None
    }

    /// Entrypoint for page navigation
    pub fn goto(&mut self, req: FrameRequestedNavigation) {
        if let Some(frame_id) = &self.main_frame {
            self.navigate_frame(frame_id.clone(), req);
        }
    }

    /// Navigate a specific frame
    pub fn navigate_frame(&mut self, frame_id: FrameId, mut req: FrameRequestedNavigation) {
        let loader_id = self.frames.get(&frame_id).and_then(|f| f.loader_id.clone());
        let watcher = NavigationWatcher::until_load(req.id, frame_id.clone(), loader_id);

        // insert the frame_id in the request if not present
        req.set_frame_id(frame_id);

        // Fresh goto — reset the per-navigation loop counter.
        self.main_frame_nav_count = 0;

        self.pending_navigations.push_back((req, watcher))
    }

    /// Fired when a frame moved to another session
    pub fn on_attached_to_target(&mut self, _event: &EventAttachedToTarget) {
        // _onFrameMoved
    }

    pub fn on_frame_tree(&mut self, frame_tree: FrameTree) {
        self.on_frame_attached(
            frame_tree.frame.id.clone(),
            frame_tree.frame.parent_id.clone(),
        );
        self.on_frame_navigated(&frame_tree.frame);
        if let Some(children) = frame_tree.child_frames {
            for child_tree in children {
                self.on_frame_tree(child_tree);
            }
        }
    }

    pub fn on_frame_attached(&mut self, frame_id: FrameId, parent_frame_id: Option<FrameId>) {
        if self.frames.contains_key(&frame_id) {
            return;
        }
        if let Some(parent_frame_id) = parent_frame_id {
            if let Some(parent_frame) = self.frames.get_mut(&parent_frame_id) {
                let frame = Frame::with_parent(frame_id.clone(), parent_frame);
                self.frames.insert(frame_id, frame);
            }
        }
    }

    pub fn on_frame_detached(&mut self, event: &EventFrameDetached) {
        self.remove_frames_recursively(&event.frame_id);
    }

    pub fn on_frame_navigated(&mut self, frame: &CdpFrame) {
        if frame.parent_id.is_some() {
            if let Some((id, mut f)) = self.frames.remove_entry(&frame.id) {
                for child in f.child_frames.drain() {
                    self.remove_frames_recursively(&child);
                }
                f.navigated(frame);
                self.frames.insert(id, f);
            }
        } else {
            // Track main-frame cross-document navigations since the last
            // `goto`. Same-document (hash change) events land in
            // `on_frame_navigated_within_document` and are not counted.
            self.main_frame_nav_count = self.main_frame_nav_count.saturating_add(1);

            let old_main = self.main_frame.take();
            let mut f = if let Some(main) = old_main.as_ref() {
                // update main frame
                if let Some(mut main_frame) = self.frames.remove(main) {
                    for child in &main_frame.child_frames {
                        self.remove_frames_recursively(child);
                    }
                    // this is necessary since we can't borrow mut and then remove recursively
                    main_frame.child_frames.clear();
                    main_frame.id = frame.id.clone();
                    main_frame
                } else {
                    Frame::new(frame.id.clone())
                }
            } else {
                // initial main frame navigation
                Frame::new(frame.id.clone())
            };
            f.navigated(frame);
            let new_id = f.id.clone();
            self.main_frame = Some(new_id.clone());
            self.frames.insert(new_id.clone(), f);

            // When the main frame ID changes (e.g. cross-origin redirect), update the
            // active navigation watcher so it tracks the new frame instead of the stale ID.
            if old_main.as_ref() != Some(&new_id) {
                if let Some((watcher, _)) = self.navigation.as_mut() {
                    if old_main.as_ref() == Some(&watcher.frame_id) {
                        watcher.frame_id = new_id;
                    }
                }
            }
        }
    }

    pub fn on_frame_navigated_within_document(&mut self, event: &EventNavigatedWithinDocument) {
        if let Some(frame) = self.frames.get_mut(&event.frame_id) {
            frame.navigated_within_url(event.url.clone());
        }
        if let Some((watcher, _)) = self.navigation.as_mut() {
            watcher.on_frame_navigated_within_document(event);
        }
    }

    pub fn on_frame_stopped_loading(&mut self, event: &EventFrameStoppedLoading) {
        if let Some(frame) = self.frames.get_mut(&event.frame_id) {
            frame.on_loading_stopped();
        }
    }

    /// Fired when frame has started loading.
    pub fn on_frame_started_loading(&mut self, event: &EventFrameStartedLoading) {
        if let Some(frame) = self.frames.get_mut(&event.frame_id) {
            frame.on_loading_started();
        }
    }

    /// Notification is issued every time when binding is called
    pub fn on_runtime_binding_called(&mut self, _ev: &EventBindingCalled) {}

    /// Issued when new execution context is created
    pub fn on_frame_execution_context_created(&mut self, event: &EventExecutionContextCreated) {
        if let Some(frame_id) = event
            .context
            .aux_data
            .as_ref()
            .and_then(|v| v["frameId"].as_str())
        {
            if let Some(frame) = self.frames.get_mut(frame_id) {
                if event
                    .context
                    .aux_data
                    .as_ref()
                    .and_then(|v| v["isDefault"].as_bool())
                    .unwrap_or_default()
                {
                    frame
                        .main_world
                        .set_context(event.context.id, event.context.unique_id.clone());
                } else if event.context.name == frame.isolated_world_name
                    && frame.secondary_world.execution_context().is_none()
                {
                    frame
                        .secondary_world
                        .set_context(event.context.id, event.context.unique_id.clone());
                }
                self.context_ids
                    .insert(event.context.unique_id.clone(), frame.id.clone());
            }
        }
        if event
            .context
            .aux_data
            .as_ref()
            .filter(|v| v["type"].as_str() == Some("isolated"))
            .is_some()
        {
            self.isolated_worlds.insert(event.context.name.clone());
        }
    }

    /// Issued when execution context is destroyed
    pub fn on_frame_execution_context_destroyed(&mut self, event: &EventExecutionContextDestroyed) {
        if let Some(id) = self.context_ids.remove(&event.execution_context_unique_id) {
            if let Some(frame) = self.frames.get_mut(&id) {
                frame.destroy_context(&event.execution_context_unique_id);
            }
        }
    }

    /// Issued when all executionContexts were cleared
    pub fn on_execution_contexts_cleared(&mut self) {
        for id in self.context_ids.values() {
            if let Some(frame) = self.frames.get_mut(id) {
                frame.clear_contexts();
            }
        }
        self.context_ids.clear();
        // Chrome just wiped every execution context, so any isolated worlds
        // we had ensured are gone too. Clearing the cache forces
        // `ensure_isolated_world` to re-issue `CreateIsolatedWorldParams` for
        // the next evaluation instead of short-circuiting on stale membership.
        self.isolated_worlds.clear();
    }

    /// Remove `context_ids` entries that reference frames which no longer
    /// exist.  Called periodically from the handler's eviction tick — a
    /// single O(n) pass instead of per-frame cleanup during recursive removal.
    pub fn evict_stale_context_ids(&mut self) {
        if !self.context_ids.is_empty() {
            self.context_ids
                .retain(|_, fid| self.frames.contains_key(fid));
        }
    }

    /// Fired for top level page lifecycle events (nav, load, paint, etc.)
    pub fn on_page_lifecycle_event(&mut self, event: &EventLifecycleEvent) {
        if let Some(frame) = self.frames.get_mut(&event.frame_id) {
            if event.name == "init" {
                frame.loader_id = Some(event.loader_id.clone());
                frame.lifecycle_events.clear();
            }
            frame.lifecycle_events.insert(event.name.clone().into());
        }
    }

    /// Detach all child frames.
    fn remove_frames_recursively(&mut self, id: &FrameId) -> Option<Frame> {
        if let Some(mut frame) = self.frames.remove(id) {
            for child in &frame.child_frames {
                self.remove_frames_recursively(child);
            }
            if let Some(parent_id) = frame.parent_frame.take() {
                if let Some(parent) = self.frames.get_mut(&parent_id) {
                    parent.child_frames.remove(&frame.id);
                }
            }
            Some(frame)
        } else {
            None
        }
    }

    pub fn ensure_isolated_world(&mut self, world_name: &str) -> Option<CommandChain> {
        if self.isolated_worlds.contains(world_name) {
            return None;
        }

        self.isolated_worlds.insert(world_name.to_string());

        if let Ok(cmd) = AddScriptToEvaluateOnNewDocumentParams::builder()
            .source(format!("//# sourceURL={}", *EVALUATION_SCRIPT_URL))
            .world_name(world_name)
            .build()
        {
            let mut cmds = Vec::with_capacity(self.frames.len() + 1);
            let identifier = cmd.identifier();

            if let Ok(cmd) = serde_json::to_value(cmd) {
                cmds.push((identifier, cmd));
            }

            let cm = self.frames.keys().filter_map(|id| {
                if let Ok(cmd) = CreateIsolatedWorldParams::builder()
                    .frame_id(id.clone())
                    .grant_univeral_access(true)
                    .world_name(world_name)
                    .build()
                {
                    let cm = (
                        cmd.identifier(),
                        serde_json::to_value(cmd).unwrap_or_default(),
                    );

                    Some(cm)
                } else {
                    None
                }
            });

            cmds.extend(cm);

            Some(CommandChain::new(cmds, self.request_timeout))
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub enum FrameEvent {
    /// A previously submitted navigation has finished
    NavigationResult(Result<NavigationOk, NavigationError>),
    /// A new navigation request needs to be submitted
    NavigationRequest(NavigationId, Request),
    /* /// The initial page of the target has been loaded
     * InitialPageLoadFinished */
}

#[derive(Debug)]
pub enum NavigationError {
    Timeout {
        id: NavigationId,
        err: DeadlineExceeded,
    },
    FrameNotFound {
        id: NavigationId,
        frame: FrameId,
    },
    /// The main frame performed more cross-document navigations during a
    /// single `goto` than the configured `max_main_frame_navigations` cap.
    /// Typically triggered by meta-refresh / `location.href` loops, which
    /// are invisible to the HTTP-layer `max_redirects` guard.
    TooManyNavigations {
        id: NavigationId,
        /// Observed main-frame navigation count (always `> cap`).
        count: u32,
    },
}

impl NavigationError {
    pub fn navigation_id(&self) -> &NavigationId {
        match self {
            NavigationError::Timeout { id, .. } => id,
            NavigationError::FrameNotFound { id, .. } => id,
            NavigationError::TooManyNavigations { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum NavigationOk {
    SameDocumentNavigation(NavigationId),
    NewDocumentNavigation(NavigationId),
}

impl NavigationOk {
    pub fn navigation_id(&self) -> &NavigationId {
        match self {
            NavigationOk::SameDocumentNavigation(id) => id,
            NavigationOk::NewDocumentNavigation(id) => id,
        }
    }
}

/// Tracks the progress of an issued `Page.navigate` request until completion.
#[derive(Debug)]
pub struct NavigationWatcher {
    id: NavigationId,
    expected_lifecycle: HashSet<MethodId>,
    frame_id: FrameId,
    loader_id: Option<LoaderId>,
    /// Once we receive the response to the issued `Page.navigate` request we
    /// can detect whether we were navigating withing the same document or were
    /// navigating to a new document by checking if a loader was included in the
    /// response.
    same_document_navigation: bool,
}

impl NavigationWatcher {
    /// Generic ctor: wait until all given lifecycle events have fired
    /// (including all child frames).
    pub fn until_lifecycle(
        id: NavigationId,
        frame: FrameId,
        loader_id: Option<LoaderId>,
        events: &[LifecycleEvent],
    ) -> Self {
        let expected_lifecycle = events.iter().map(LifecycleEvent::to_method_id).collect();

        Self {
            id,
            expected_lifecycle,
            frame_id: frame,
            loader_id,
            same_document_navigation: false,
        }
    }

    /// Wait for "load"
    pub fn until_load(id: NavigationId, frame: FrameId, loader_id: Option<LoaderId>) -> Self {
        Self::until_lifecycle(id, frame, loader_id, &[LifecycleEvent::Load])
    }

    /// Wait for DOMContentLoaded
    pub fn until_domcontent_loaded(
        id: NavigationId,
        frame: FrameId,
        loader_id: Option<LoaderId>,
    ) -> Self {
        Self::until_lifecycle(id, frame, loader_id, &[LifecycleEvent::DomcontentLoaded])
    }

    /// Wait for networkIdle
    pub fn until_network_idle(
        id: NavigationId,
        frame: FrameId,
        loader_id: Option<LoaderId>,
    ) -> Self {
        Self::until_lifecycle(id, frame, loader_id, &[LifecycleEvent::NetworkIdle])
    }

    /// Wait for networkAlmostIdle
    pub fn until_network_almost_idle(
        id: NavigationId,
        frame: FrameId,
        loader_id: Option<LoaderId>,
    ) -> Self {
        Self::until_lifecycle(id, frame, loader_id, &[LifecycleEvent::NetworkAlmostIdle])
    }

    /// (optional) Wait for multiple states, e.g. DOMContentLoaded + networkIdle
    pub fn until_domcontent_and_network_idle(
        id: NavigationId,
        frame: FrameId,
        loader_id: Option<LoaderId>,
    ) -> Self {
        Self::until_lifecycle(
            id,
            frame,
            loader_id,
            &[
                LifecycleEvent::DomcontentLoaded,
                LifecycleEvent::NetworkIdle,
            ],
        )
    }

    /// Checks whether the navigation was completed
    pub fn is_lifecycle_complete(&self) -> bool {
        self.expected_lifecycle.is_empty()
    }

    fn on_frame_navigated_within_document(&mut self, ev: &EventNavigatedWithinDocument) {
        if self.frame_id == ev.frame_id {
            self.same_document_navigation = true;
        }
    }
}

/// An identifier for an ongoing navigation
#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
pub struct NavigationId(pub usize);

/// Represents a the request for a navigation
#[derive(Debug)]
pub struct FrameRequestedNavigation {
    /// The internal identifier
    pub id: NavigationId,
    /// the cdp request that will trigger the navigation
    pub req: Request,
    /// The timeout after which the request will be considered timed out
    pub timeout: Duration,
}

impl FrameRequestedNavigation {
    pub fn new(id: NavigationId, req: Request, request_timeout: Duration) -> Self {
        Self {
            id,
            req,
            timeout: request_timeout,
        }
    }

    /// This will set the id of the frame into the `params` `frameId` field.
    pub fn set_frame_id(&mut self, frame_id: FrameId) {
        if let Some(params) = self.req.params.as_object_mut() {
            if let Entry::Vacant(entry) = params.entry("frameId") {
                entry.insert(serde_json::Value::String(frame_id.into()));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LifecycleEvent {
    #[default]
    Load,
    DomcontentLoaded,
    NetworkIdle,
    NetworkAlmostIdle,
}

impl LifecycleEvent {
    #[inline]
    pub fn to_method_id(&self) -> MethodId {
        match self {
            LifecycleEvent::Load => "load".into(),
            LifecycleEvent::DomcontentLoaded => "DOMContentLoaded".into(),
            LifecycleEvent::NetworkIdle => "networkIdle".into(),
            LifecycleEvent::NetworkAlmostIdle => "networkAlmostIdle".into(),
        }
    }
}

impl AsRef<str> for LifecycleEvent {
    fn as_ref(&self) -> &str {
        match self {
            LifecycleEvent::Load => "load",
            LifecycleEvent::DomcontentLoaded => "DOMContentLoaded",
            LifecycleEvent::NetworkIdle => "networkIdle",
            LifecycleEvent::NetworkAlmostIdle => "networkAlmostIdle",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_lifecycle_events_cleared_on_loading_started() {
        let mut frame = Frame::new(FrameId::new("test"));

        // Simulate a loaded page.
        frame.lifecycle_events.insert("load".into());
        frame.lifecycle_events.insert("DOMContentLoaded".into());
        assert!(frame.is_loaded());

        // Browser fires FrameStartedLoading → on_loading_started clears lifecycle.
        frame.on_loading_started();
        assert!(!frame.is_loaded());
    }

    #[test]
    fn frame_loading_stopped_inserts_load_events() {
        let mut frame = Frame::new(FrameId::new("test"));
        assert!(!frame.is_loaded());

        frame.on_loading_stopped();
        assert!(frame.is_loaded());
    }

    #[test]
    fn navigation_completes_when_main_frame_loaded_despite_child_frames() {
        let timeout = Duration::from_secs(30);
        let mut fm = FrameManager::new(timeout);

        // Set up main frame with "load" lifecycle event.
        let main_id = FrameId::new("main");
        let mut main_frame = Frame::new(main_id.clone());
        main_frame.loader_id = Some(LoaderId::from("loader-old".to_string()));
        main_frame.lifecycle_events.insert("load".into());
        fm.frames.insert(main_id.clone(), main_frame);
        fm.main_frame = Some(main_id.clone());

        // Attach a child frame that has NOT received "load" (e.g. a stuck ad iframe).
        let child_id = FrameId::new("child-ad");
        let child = Frame::with_parent(child_id.clone(), fm.frames.get_mut(&main_id).unwrap());
        fm.frames.insert(child_id, child);

        // Build a watcher that waits for "load" on the main frame.
        let watcher = NavigationWatcher::until_load(
            NavigationId(0),
            main_id.clone(),
            Some(LoaderId::from("loader-old".to_string())),
        );

        // Simulate a new loader (navigation happened).
        fm.frames.get_mut(&main_id).unwrap().loader_id =
            Some(LoaderId::from("loader-new".to_string()));

        // Navigation should complete because main frame has "load",
        // even though the child frame does not.
        let main_frame = fm.frames.get(&main_id).unwrap();
        let result = fm.check_lifecycle_complete(&watcher, main_frame);
        assert!(
            result.is_some(),
            "navigation should complete without waiting for child frames"
        );
    }

    #[test]
    fn navigation_watcher_tracks_main_frame_id_change() {
        let timeout = Duration::from_secs(30);
        let mut fm = FrameManager::new(timeout);

        // Set up main frame with old ID.
        let old_id = FrameId::new("old-main");
        let mut main_frame = Frame::new(old_id.clone());
        main_frame.loader_id = Some(LoaderId::from("loader-1".to_string()));
        fm.frames.insert(old_id.clone(), main_frame);
        fm.main_frame = Some(old_id.clone());

        // Manually insert a navigation watcher referencing the old frame ID
        // (simulates what navigate_frame does after queuing a request).
        let watcher = NavigationWatcher::until_load(
            NavigationId(0),
            old_id.clone(),
            Some(LoaderId::from("loader-1".to_string())),
        );
        let deadline = Instant::now() + timeout;
        fm.navigation = Some((watcher, deadline));

        // Simulate cross-origin redirect: main frame ID changes.
        // Directly manipulate the frame map to simulate on_frame_navigated
        // with a new main frame ID (avoids constructing the full CdpFrame).
        let new_id = FrameId::new("new-main");
        if let Some(mut old_frame) = fm.frames.remove(&old_id) {
            old_frame.child_frames.clear();
            old_frame.id = new_id.clone();
            fm.frames.insert(new_id.clone(), old_frame);
        }
        fm.main_frame = Some(new_id.clone());

        // Update the watcher the same way on_frame_navigated does.
        if let Some((watcher, _)) = fm.navigation.as_mut() {
            if watcher.frame_id == old_id {
                watcher.frame_id = new_id.clone();
            }
        }

        // The active watcher should now track the new frame ID.
        let (watcher, _) = fm.navigation.as_ref().unwrap();
        assert_eq!(
            watcher.frame_id, new_id,
            "watcher should follow the main frame ID change"
        );

        // Simulate lifecycle events on the new frame so navigation completes.
        fm.frames.get_mut(&new_id).unwrap().loader_id =
            Some(LoaderId::from("loader-2".to_string()));
        fm.frames
            .get_mut(&new_id)
            .unwrap()
            .lifecycle_events
            .insert("load".into());

        let event = fm.poll(Instant::now());
        assert!(
            matches!(event, Some(FrameEvent::NavigationResult(Ok(_)))),
            "navigation should complete on the new frame"
        );
    }

    // ── Main-frame navigation-loop guard ─────────────────────────────

    fn seed_main_frame(fm: &mut FrameManager, loader: &str) -> FrameId {
        let id = FrameId::new("main");
        let mut frame = Frame::new(id.clone());
        frame.loader_id = Some(LoaderId::from(loader.to_string()));
        fm.frames.insert(id.clone(), frame);
        fm.main_frame = Some(id.clone());
        id
    }

    fn active_watcher(fm: &mut FrameManager, frame_id: FrameId) {
        let watcher = NavigationWatcher::until_load(
            NavigationId(0),
            frame_id.clone(),
            fm.frames.get(&frame_id).and_then(|f| f.loader_id.clone()),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        fm.navigation = Some((watcher, deadline));
    }

    #[test]
    fn nav_loop_guard_none_allows_unlimited() {
        let mut fm = FrameManager::new(Duration::from_secs(30));
        // Default: max_main_frame_navigations = None.
        let id = seed_main_frame(&mut fm, "loader-0");
        active_watcher(&mut fm, id);

        // Simulate 25 main-frame navigations — no cap → no error.
        for _ in 0..25 {
            fm.main_frame_nav_count = fm.main_frame_nav_count.saturating_add(1);
        }

        // poll should not trip on count; only deadline/lifecycle drive it.
        // (Lifecycle is incomplete so poll returns None here, but critically
        // no NavigationError is emitted.)
        let event = fm.poll(Instant::now());
        assert!(
            !matches!(
                event,
                Some(FrameEvent::NavigationResult(Err(
                    NavigationError::TooManyNavigations { .. }
                )))
            ),
            "None cap must never emit TooManyNavigations"
        );
    }

    #[test]
    fn nav_loop_guard_caps_and_reports_count() {
        let mut fm = FrameManager::new(Duration::from_secs(30));
        fm.set_max_main_frame_navigations(Some(3));
        let id = seed_main_frame(&mut fm, "loader-0");
        active_watcher(&mut fm, id);

        // Simulate 5 main-frame navigations — cap is 3, so 4+ trips.
        for _ in 0..5 {
            fm.main_frame_nav_count = fm.main_frame_nav_count.saturating_add(1);
        }

        match fm.poll(Instant::now()) {
            Some(FrameEvent::NavigationResult(Err(NavigationError::TooManyNavigations {
                count,
                ..
            }))) => {
                assert_eq!(count, 5, "reported count must be the observed value");
            }
            other => panic!("expected TooManyNavigations, got {other:?}"),
        }
    }

    #[test]
    fn nav_loop_guard_resets_on_goto() {
        let mut fm = FrameManager::new(Duration::from_secs(30));
        fm.set_max_main_frame_navigations(Some(3));
        let id = seed_main_frame(&mut fm, "loader-0");
        active_watcher(&mut fm, id.clone());

        // Trip the cap on the first goto.
        fm.main_frame_nav_count = 10;
        assert!(matches!(
            fm.poll(Instant::now()),
            Some(FrameEvent::NavigationResult(Err(
                NavigationError::TooManyNavigations { .. }
            )))
        ));

        // Fresh goto must reset the counter.
        fm.navigate_frame(
            id.clone(),
            FrameRequestedNavigation::new(
                NavigationId(1),
                Request::new("Page.navigate".into(), serde_json::json!({})),
                Duration::from_secs(30),
            ),
        );
        assert_eq!(
            fm.main_frame_nav_count, 0,
            "navigate_frame must reset the main-frame nav counter"
        );
    }

    #[test]
    fn nav_loop_guard_same_document_not_counted() {
        // Same-document navigations (hash changes, History.pushState) land in
        // `on_frame_navigated_within_document`, not `on_frame_navigated`, so
        // they must NOT increment the cross-document counter. This test
        // encodes that contract — if `on_frame_navigated_within_document`
        // ever starts incrementing `main_frame_nav_count`, it fails.
        let mut fm = FrameManager::new(Duration::from_secs(30));
        fm.set_max_main_frame_navigations(Some(2));
        let id = seed_main_frame(&mut fm, "loader-0");

        // Seed an active navigation watcher on this frame.
        active_watcher(&mut fm, id.clone());

        // Dispatch 10 same-document navigations.
        for _ in 0..10 {
            fm.on_frame_navigated_within_document(
                &chromiumoxide_cdp::cdp::browser_protocol::page::EventNavigatedWithinDocument {
                    frame_id: id.clone(),
                    url: "https://example.com/#a".into(),
                    navigation_type:
                        chromiumoxide_cdp::cdp::browser_protocol::page::NavigatedWithinDocumentNavigationType::Fragment,
                },
            );
        }

        assert_eq!(
            fm.main_frame_nav_count, 0,
            "same-document navigations must not count against the cross-document cap"
        );
    }

    #[test]
    fn execution_contexts_cleared_resets_isolated_worlds() {
        // When Chrome fires `executionContextsCleared`, the isolated worlds
        // we had ensured are gone along with every execution context.
        // `isolated_worlds` must be cleared so the next `ensure_isolated_world`
        // call re-issues the creation command rather than short-circuiting on
        // stale membership.
        let mut fm = FrameManager::new(Duration::from_secs(30));

        let frame_id = FrameId::new("main");
        let frame = Frame::new(frame_id.clone());
        let world_name = frame.get_isolated_world_name().clone();
        fm.frames.insert(frame_id.clone(), frame);
        fm.main_frame = Some(frame_id);

        // First call: the world isn't in the set, so we should produce a
        // command chain AND record membership.
        let first = fm.ensure_isolated_world(&world_name);
        assert!(
            first.is_some(),
            "first ensure_isolated_world must emit a creation command chain"
        );
        assert!(
            fm.isolated_worlds.contains(&world_name),
            "isolated_worlds must record the ensured world"
        );

        // Second call: short-circuits because the world is already ensured.
        let second = fm.ensure_isolated_world(&world_name);
        assert!(
            second.is_none(),
            "second ensure_isolated_world must short-circuit while membership is present"
        );

        // Chrome signals that every execution context was wiped.
        fm.on_execution_contexts_cleared();
        assert!(
            fm.context_ids.is_empty(),
            "context_ids must be cleared after executionContextsCleared"
        );
        assert!(
            fm.isolated_worlds.is_empty(),
            "isolated_worlds must be cleared after executionContextsCleared"
        );

        // Third call: must re-issue the creation command because the isolated
        // world no longer exists in Chrome.
        let third = fm.ensure_isolated_world(&world_name);
        assert!(
            third.is_some(),
            "ensure_isolated_world must re-emit a creation chain after a context wipe"
        );
        assert!(
            fm.isolated_worlds.contains(&world_name),
            "isolated_worlds must re-record the world after re-ensuring"
        );
    }
}
