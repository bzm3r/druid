// Copyright 2018 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Simple entity-component-system based GUI.

pub use druid_shell::{self as shell, kurbo, piet};

use std::any::Any;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::time::Instant;

use kurbo::{Point, Rect, Size, Vec2};
use piet::{Color, Piet, RenderContext};

use druid_shell::application::Application;
pub use druid_shell::dialog::{FileDialogOptions, FileDialogType};
pub use druid_shell::keyboard::{KeyCode, KeyEvent, KeyModifiers};
use druid_shell::platform::IdleHandle;
use druid_shell::window::{self, WinHandler, WindowHandle};

mod graph;
pub mod widget;

use graph::Graph;
use widget::NullWidget;
pub use widget::{MouseEvent, Widget};

//FIXME: this should come from a theme or environment at some point.
const BACKGROUND_COLOR: Color = Color::rgb24(0x27_28_22);

/// The top-level handler for the UI.
///
/// This struct ultimately has ownership of all components within the UI.
/// It implements the `WinHandler` trait of druid-win-shell, and, after the
/// UI is built, ownership is transferred to the window, through `set_handler`
/// in the druid-win-shell window building sequence.
pub struct UiMain {
    state: RefCell<UiState>,
}

/// An identifier for widgets, scoped to a UiMain instance. This is the
/// "entity" of the entity-component-system architecture.
pub type Id = usize;

pub struct UiState {
    listeners: BTreeMap<Id, Vec<Box<dyn FnMut(&mut dyn Any, ListenerCtx)>>>,

    command_listener: Option<Box<dyn FnMut(u32, ListenerCtx)>>,

    /// The widget tree and associated state is split off into a separate struct
    /// so that we can use a mutable reference to it as the listener context.
    inner: Ui,
}

/// This struct is being renamed.
#[deprecated]
pub type UiInner = Ui;

/// The main access point for manipulating the UI.
pub struct Ui {
    /// The individual widget trait objects.
    widgets: Vec<Box<dyn Widget>>,

    /// Graph of widgets (actually a strict tree structure, so maybe should be renamed).
    graph: Graph,

    /// The state (other than widget tree) is a separate object, so that a
    /// mutable reference to it can be used as a layout context.
    layout_ctx: LayoutCtx,
}

/// The context given to layout methods.
pub struct LayoutCtx {
    handle: WindowHandle,

    /// Bounding box of each widget. The position is relative to the parent.
    geom: Vec<Rect>,

    /// Additional state per widget.
    ///
    /// A case can be made to fold `geom` here instead of having a separate array;
    /// this is the general SOA vs AOS discussion.
    per_widget: Vec<PerWidgetState>,

    /// State of animation requests.
    anim_state: AnimState,

    /// The time of the last paint cycle.
    prev_paint_time: Option<Instant>,

    /// Queue of events to dispatch after build or handler.
    event_q: Vec<Event>,

    /// Which widget is currently focused, if any.
    focused: Option<Id>,

    /// Which widget is active (mouse is pressed), if any.
    active: Option<Id>,

    /// Which widget is hot (hovered), if any.
    hot: Option<Id>,

    /// The size of the paint surface
    size: Size,
}

#[deprecated(note = "please use `Rect` directly.")]
pub type Geometry = Rect;

#[derive(Default)]
struct PerWidgetState {
    anim_frame_requested: bool,
}

enum AnimState {
    Idle,
    InvalidationRequested,
    AnimFrameStart,
    AnimFrameRequested,
}

#[derive(Clone, Copy, Debug)]
pub struct BoxConstraints {
    min: Size,
    max: Size,
}

pub enum LayoutResult {
    Size(Size),
    RequestChild(Id, BoxConstraints),
}

enum Event {
    /// Event to be delivered to listeners.
    Event(Id, Box<dyn Any>),

    /// A request to add a listener.
    AddListener(Id, Box<dyn FnMut(&mut dyn Any, ListenerCtx)>),

    /// Sent when a widget is removed so its listeners can be deleted.
    ClearListeners(Id),
}

// Contexts for widget methods.

/// Context given to handlers.
pub struct HandlerCtx<'a> {
    /// The id of the node sending the event
    id: Id,

    layout_ctx: &'a mut LayoutCtx,
}

/// The context given to listeners.
///
/// Listeners are allowed to poke widgets and mutate the graph.
pub struct ListenerCtx<'a> {
    id: Id,

    inner: &'a mut Ui,
}

pub struct PaintCtx<'a, 'b: 'a> {
    // TODO: maybe this should be a 3-way enum: normal/hot/active
    is_active: bool,
    is_hot: bool,
    is_focused: bool,
    pub render_ctx: &'a mut Piet<'b>,
}

#[derive(Debug)]
pub enum Error {
    ShellError(druid_shell::Error),
}

impl From<druid_shell::Error> for Error {
    fn from(e: druid_shell::Error) -> Error {
        Error::ShellError(e)
    }
}

impl UiMain {
    pub fn new(state: UiState) -> UiMain {
        UiMain {
            state: RefCell::new(state),
        }
    }

    /// Send an event to a specific widget. This calls the widget's `poke` method
    /// at some time in the future.
    pub fn send_ext<A: Any + Send>(idle_handle: &IdleHandle, id: Id, a: A) {
        let mut boxed_a = Box::new(a);
        idle_handle.add_idle(move |a| {
            let ui_main = a.downcast_ref::<UiMain>().unwrap();
            let mut state = ui_main.state.borrow_mut();
            state.poke(id, boxed_a.deref_mut());
        });
    }
}

impl UiState {
    pub fn new() -> UiState {
        UiState {
            listeners: Default::default(),
            command_listener: None,
            inner: Ui {
                widgets: Vec::new(),
                graph: Default::default(),
                layout_ctx: LayoutCtx {
                    geom: Vec::new(),
                    per_widget: Vec::new(),
                    anim_state: AnimState::Idle,
                    prev_paint_time: None,
                    handle: Default::default(),
                    event_q: Vec::new(),
                    focused: None,
                    active: None,
                    hot: None,
                    size: Size::ZERO,
                },
            },
        }
    }

    /// Set a listener for menu commands.
    pub fn set_command_listener<F>(&mut self, f: F)
    where
        F: FnMut(u32, ListenerCtx) + 'static,
    {
        self.command_listener = Some(Box::new(f));
    }

    fn mouse(&mut self, pos: Point, raw_event: &window::MouseEvent) {
        fn dispatch_mouse(
            widgets: &mut [Box<dyn Widget>],
            node: Id,
            pos: Point,
            raw_event: &window::MouseEvent,
            ctx: &mut HandlerCtx,
        ) -> bool {
            let event = MouseEvent {
                pos,
                mods: raw_event.mods,
                button: raw_event.button,
                count: raw_event.count,
            };
            widgets[node].mouse(&event, ctx)
        }

        fn mouse_rec(
            widgets: &mut [Box<dyn Widget>],
            graph: &Graph,
            pos: Point,
            raw_event: &window::MouseEvent,
            ctx: &mut HandlerCtx,
        ) -> bool {
            let node = ctx.id;
            let g = ctx.layout_ctx.geom[node];
            let Vec2 { x, y } = pos - g.origin();
            let Size { width, height } = g.size();
            let mut handled = false;
            if x >= 0.0 && y >= 0.0 && x < width && y < height {
                handled = dispatch_mouse(widgets, node, Point::new(x, y), raw_event, ctx);
                for child in graph.children[node].iter().rev() {
                    if handled {
                        break;
                    }
                    ctx.id = *child;
                    handled = mouse_rec(widgets, graph, Point::new(x, y), raw_event, ctx);
                }
            }
            handled
        }

        if let Some(active) = self.layout_ctx.active {
            // Send mouse event directly to active widget.
            let pos = pos - self.offset_of_widget(active);
            dispatch_mouse(
                &mut self.inner.widgets,
                active,
                pos,
                raw_event,
                &mut HandlerCtx {
                    id: active,
                    layout_ctx: &mut self.inner.layout_ctx,
                },
            );
        } else {
            mouse_rec(
                &mut self.inner.widgets,
                &self.inner.graph,
                pos,
                raw_event,
                &mut HandlerCtx {
                    id: self.inner.graph.root,
                    layout_ctx: &mut self.inner.layout_ctx,
                },
            );
        }
        self.dispatch_events();
    }

    fn mouse_move(&mut self, pos: Point) {
        // Note: this logic is similar to that for hit testing on mouse, but is
        // slightly different if child geom's overlap. Maybe we reconcile them,
        // maybe it's fine.
        let mut node = self.graph.root;
        let mut new_hot = None;
        let mut tpos = pos;
        loop {
            let g = self.layout_ctx.geom[node];
            tpos -= g.origin().to_vec2();
            if self.graph.children[node].is_empty() {
                new_hot = Some(node);
                break;
            }
            let mut child_hot = None;
            for child in self.graph.children[node].iter().rev() {
                let child_g = self.layout_ctx.geom[*child];
                let cpos = tpos - child_g.origin();
                let Size { width, height } = child_g.size();

                //FIXME: when kurbo 0.3.2 lands, we can write:
                // if child_g.with_origin(Point::ORIGIN).contains(cpos)
                if cpos.x >= 0.0 && cpos.y >= 0.0 && cpos.x < width && cpos.y < height {
                    child_hot = Some(child);
                    break;
                }
            }
            if let Some(child) = child_hot {
                node = *child;
            } else {
                break;
            }
        }
        let old_hot = self.layout_ctx.hot;
        if new_hot != old_hot {
            self.layout_ctx.hot = new_hot;
            if let Some(old_hot) = old_hot {
                self.inner.widgets[old_hot].on_hot_changed(
                    false,
                    &mut HandlerCtx {
                        id: old_hot,
                        layout_ctx: &mut self.inner.layout_ctx,
                    },
                );
            }
            if let Some(new_hot) = new_hot {
                self.inner.widgets[new_hot].on_hot_changed(
                    true,
                    &mut HandlerCtx {
                        id: new_hot,
                        layout_ctx: &mut self.inner.layout_ctx,
                    },
                );
            }
        }

        if let Some(node) = self.layout_ctx.active.or(new_hot) {
            let pos = pos - self.offset_of_widget(node);
            self.inner.widgets[node].mouse_moved(
                pos,
                &mut HandlerCtx {
                    id: node,
                    layout_ctx: &mut self.inner.layout_ctx,
                },
            );
        }
        self.dispatch_events();
    }

    fn handle_key_down(&mut self, event: &KeyEvent) -> bool {
        if let Some(id) = self.layout_ctx.focused {
            let handled = {
                let mut ctx = HandlerCtx {
                    id,
                    layout_ctx: &mut self.inner.layout_ctx,
                };
                self.inner.widgets[id].key_down(event, &mut ctx)
            };
            self.dispatch_events();
            handled
        } else {
            false
        }
    }

    fn handle_key_up(&mut self, event: &KeyEvent) {
        if let Some(id) = self.layout_ctx.focused {
            let mut ctx = HandlerCtx {
                id,
                layout_ctx: &mut self.inner.layout_ctx,
            };
            self.inner.widgets[id].key_up(event, &mut ctx);
            self.dispatch_events();
        }
    }

    fn handle_scroll(&mut self, event: &window::ScrollEvent) {
        if let Some(id) = self.layout_ctx.hot {
            let mut ctx = HandlerCtx {
                id,
                layout_ctx: &mut self.inner.layout_ctx,
            };
            self.inner.widgets[id].scroll(event, &mut ctx);
            self.dispatch_events();
        }
    }

    fn handle_command(&mut self, cmd: u32) {
        if let Some(ref mut listener) = self.command_listener {
            let ctx = ListenerCtx {
                id: self.inner.graph.root,
                inner: &mut self.inner,
            };
            listener(cmd, ctx);
        } else {
            println!("command received but no handler");
        }
    }

    fn dispatch_events(&mut self) {
        while !self.layout_ctx.event_q.is_empty() {
            let event_q = mem::replace(&mut self.layout_ctx.event_q, Vec::new());
            for event in event_q {
                match event {
                    Event::Event(id, mut event) => {
                        if let Some(listeners) = self.listeners.get_mut(&id) {
                            for listener in listeners {
                                let ctx = ListenerCtx {
                                    id,
                                    inner: &mut self.inner,
                                };
                                listener(event.deref_mut(), ctx);
                            }
                        }
                    }
                    Event::AddListener(id, listener) => {
                        self.listeners.entry(id).or_default().push(listener);
                    }
                    Event::ClearListeners(id) => {
                        self.listeners.get_mut(&id).map(|l| l.clear());
                    }
                }
            }
        }
    }

    // Process an animation frame. This consists mostly of calling anim_frame on
    // widgets that have requested a frame.
    fn anim_frame(&mut self) {
        // TODO: this is just wall-clock time, which will have jitter making
        // animations not as smooth. Should be extracting actual refresh rate
        // from presentation statistics and then doing some processing.
        let this_paint_time = Instant::now();
        let interval = if let Some(last) = self.layout_ctx.prev_paint_time {
            let duration = this_paint_time.duration_since(last);
            1_000_000_000 * duration.as_secs() + (duration.subsec_nanos() as u64)
        } else {
            0
        };
        self.layout_ctx.anim_state = AnimState::AnimFrameStart;
        for node in 0..self.widgets.len() {
            if self.layout_ctx.per_widget[node].anim_frame_requested {
                self.layout_ctx.per_widget[node].anim_frame_requested = false;
                self.inner.widgets[node].anim_frame(
                    interval,
                    &mut HandlerCtx {
                        id: node,
                        layout_ctx: &mut self.inner.layout_ctx,
                    },
                );
            }
        }
        self.layout_ctx.prev_paint_time = Some(this_paint_time);
        self.dispatch_events();
    }

    /// Returns a `Vec2` representing the position of this node relative
    /// to the origin.
    fn offset_of_widget(&mut self, mut node: Id) -> Vec2 {
        let mut delta = Vec2::default();
        loop {
            let g = self.layout_ctx.geom[node];
            delta += g.origin().to_vec2();
            let parent = self.graph.parent[node];
            if parent == node {
                break;
            }
            node = parent;
        }
        delta
    }
}

impl Deref for UiState {
    type Target = Ui;

    fn deref(&self) -> &Ui {
        &self.inner
    }
}

impl DerefMut for UiState {
    fn deref_mut(&mut self) -> &mut Ui {
        &mut self.inner
    }
}

impl Ui {
    /// Send an arbitrary payload to a widget. The type and interpretation of the
    /// payload depends on the specific target widget.
    pub fn poke<A: Any>(&mut self, node: Id, payload: &mut A) -> bool {
        let mut ctx = HandlerCtx {
            id: node,
            layout_ctx: &mut self.layout_ctx,
        };
        self.widgets[node].poke(payload, &mut ctx)
    }

    /// Put a widget in the graph and add its children. Returns newly allocated
    /// id for the node.
    pub fn add<W>(&mut self, widget: W, children: &[Id]) -> Id
    where
        W: Widget + 'static,
    {
        let id = self.graph.alloc_node();
        if id < self.widgets.len() {
            self.widgets[id] = Box::new(widget);
            self.layout_ctx.geom[id] = Default::default();
            self.layout_ctx.per_widget[id] = Default::default();
        } else {
            self.widgets.push(Box::new(widget));
            self.layout_ctx.geom.push(Default::default());
            self.layout_ctx.per_widget.push(Default::default());
        }
        for &child in children {
            self.graph.append_child(id, child);
        }
        id
    }

    pub fn set_root(&mut self, root: Id) {
        self.graph.root = root;
    }

    /// Set the focused widget.
    pub fn set_focus(&mut self, node: Option<Id>) {
        self.layout_ctx.focused = node;
    }

    /// Add a listener that expects a specific type.
    pub fn add_listener<A, F>(&mut self, node: Id, mut f: F)
    where
        A: Any,
        F: FnMut(&mut A, ListenerCtx) + 'static,
    {
        let wrapper: Box<dyn FnMut(&mut dyn Any, ListenerCtx)> = Box::new(move |a, ctx| {
            if let Some(arg) = a.downcast_mut() {
                f(arg, ctx)
            } else {
                println!("type mismatch in listener arg");
            }
        });
        self.layout_ctx
            .event_q
            .push(Event::AddListener(node, wrapper));
    }

    /// Add a child dynamically, in the last position.
    pub fn append_child(&mut self, node: Id, child: Id) {
        // TODO: could do some validation of graph structure (cycles would be bad).
        self.graph.append_child(node, child);
        self.layout_ctx.request_layout();
    }

    /// Add a child dynamically, before the given sibling.
    pub fn add_before(&mut self, node: Id, sibling: Id, child: Id) {
        self.graph.add_before(node, sibling, child);
        self.layout_ctx.request_layout();
    }

    /// Remove a child.
    ///
    /// Can panic if child is not a valid child. The child is not deleted, but
    /// can be added again later. The listeners for the child are not cleared.
    pub fn remove_child(&mut self, node: Id, child: Id) {
        self.graph.remove_child(node, child);
        self.widgets[node].on_child_removed(child);
        self.layout_ctx.request_layout();
    }

    /// Delete a child.
    ///
    /// Can panic if child is not a valid child. Deletes the subtree rooted at
    /// the child, drops those widgets, and clears all listeners.

    /// The id of the child may be reused; callers should take care not to use the
    /// child id in any way afterwards.
    pub fn delete_child(&mut self, node: Id, child: Id) {
        fn delete_rec(
            widgets: &mut [Box<dyn Widget>],
            q: &mut Vec<Event>,
            graph: &Graph,
            node: Id,
        ) {
            widgets[node] = Box::new(NullWidget);
            q.push(Event::ClearListeners(node));
            for &child in &graph.children[node] {
                delete_rec(widgets, q, graph, child);
            }
        }
        delete_rec(
            &mut self.widgets,
            &mut self.layout_ctx.event_q,
            &self.graph,
            child,
        );
        self.remove_child(node, child);
        self.graph.free_subtree(child);
    }

    // The following methods are really UiState methods, but don't need access to listeners
    // so are more concise to implement here.

    fn paint(&mut self, render_ctx: &mut Piet, root: Id) {
        // Do pre-order traversal on graph, painting each node in turn.
        //
        // Implemented as a recursion, but we could use an explicit queue instead.
        fn paint_rec(
            widgets: &mut [Box<dyn Widget>],
            graph: &Graph,
            geom: &[Rect],
            paint_ctx: &mut PaintCtx,
            node: Id,
            pos: Point,
            active: Option<Id>,
            hot: Option<Id>,
            focused: Option<Id>,
        ) {
            let g = geom[node] + pos.to_vec2();
            paint_ctx.is_active = active == Some(node);
            paint_ctx.is_hot = hot == Some(node) && (paint_ctx.is_active || active.is_none());
            paint_ctx.is_focused = focused == Some(node);
            widgets[node].paint(paint_ctx, &g);
            for &child in &graph.children[node] {
                let pos = g.origin();
                paint_rec(
                    widgets, graph, geom, paint_ctx, child, pos, active, hot, focused,
                );
            }
        }

        let mut paint_ctx = PaintCtx {
            is_active: false,
            is_hot: false,
            is_focused: false,
            render_ctx,
        };
        paint_rec(
            &mut self.widgets,
            &self.graph,
            &self.layout_ctx.geom,
            &mut paint_ctx,
            root,
            Point::ORIGIN,
            self.layout_ctx.active,
            self.layout_ctx.hot,
            self.layout_ctx.focused,
        );
    }

    fn layout(&mut self, bc: &BoxConstraints, root: Id) {
        fn layout_rec(
            widgets: &mut [Box<dyn Widget>],
            ctx: &mut LayoutCtx,
            graph: &Graph,
            bc: &BoxConstraints,
            node: Id,
        ) -> Size {
            let mut size = None;
            loop {
                let layout_res = widgets[node].layout(bc, &graph.children[node], size, ctx);
                match layout_res {
                    LayoutResult::Size(size) => {
                        ctx.geom[node] = ctx.geom[node].with_size(size);
                        return size;
                    }
                    LayoutResult::RequestChild(child, child_bc) => {
                        size = Some(layout_rec(widgets, ctx, graph, &child_bc, child));
                    }
                }
            }
        }

        layout_rec(
            &mut self.widgets,
            &mut self.layout_ctx,
            &self.graph,
            bc,
            root,
        );
    }
}

impl BoxConstraints {
    pub fn new(min: Size, max: Size) -> BoxConstraints {
        BoxConstraints { min, max }
    }

    pub fn tight(size: Size) -> BoxConstraints {
        BoxConstraints {
            min: size,
            max: size,
        }
    }

    pub fn constrain(&self, size: impl Into<Size>) -> Size {
        size.into().clamp(self.min, self.max)
    }

    /// Returns the max size of these constraints.
    pub fn max(&self) -> Size {
        self.max
    }

    /// Returns the min size of these constraints.
    pub fn min(&self) -> Size {
        self.min
    }
}

impl LayoutCtx {
    pub fn position_child(&mut self, child: Id, pos: impl Into<Point>) {
        self.geom[child] = self.geom[child].with_origin(pos.into());
    }

    pub fn get_child_size(&self, child: Id) -> Size {
        self.geom[child].size()
    }

    /// Internal logic for widget invalidation.
    fn invalidate(&mut self) {
        match self.anim_state {
            AnimState::Idle => {
                self.handle.invalidate();
                self.anim_state = AnimState::InvalidationRequested;
            }
            _ => (),
        }
    }

    fn request_layout(&mut self) {
        self.invalidate();
    }
}

impl<'a> HandlerCtx<'a> {
    /// Invalidate this widget. Finer-grained invalidation is not yet implemented,
    /// but when it is, this method will invalidate the widget's bounding box.
    pub fn invalidate(&mut self) {
        self.layout_ctx.invalidate();
    }

    /// Request layout; implies invalidation.
    pub fn request_layout(&mut self) {
        self.layout_ctx.request_layout();
    }

    /// Send an event, to be handled by listeners.
    pub fn send_event<A: Any>(&mut self, a: A) {
        self.layout_ctx
            .event_q
            .push(Event::Event(self.id, Box::new(a)));
    }

    /// Set or unset the widget as active.
    // TODO: this should call SetCapture/ReleaseCapture as well.
    pub fn set_active(&mut self, active: bool) {
        self.layout_ctx.active = if active { Some(self.id) } else { None };
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.layout_ctx.focused = if focused { Some(self.id) } else { None };
    }

    /// Determine whether this widget is active.
    pub fn is_active(&self) -> bool {
        self.layout_ctx.active == Some(self.id)
    }

    /// Determine whether this widget is focused.
    pub fn is_focused(&self) -> bool {
        self.layout_ctx.focused == Some(self.id)
    }

    /// Determine whether this widget is hot. A widget can be both hot and active, but
    /// if a widget is active, it is the only widget that can be hot.
    pub fn is_hot(&self) -> bool {
        self.layout_ctx.hot == Some(self.id)
            && (self.is_active() || self.layout_ctx.active.is_none())
    }

    /// Request an animation frame.
    ///
    /// Calling this schedules an animation frame, and also causes `anim_frame` to be
    /// called on this widget at the beginning of that frame.
    pub fn request_anim_frame(&mut self) {
        self.layout_ctx.per_widget[self.id].anim_frame_requested = true;
        match self.layout_ctx.anim_state {
            AnimState::Idle => {
                self.invalidate();
            }
            AnimState::AnimFrameStart => {
                self.layout_ctx.anim_state = AnimState::AnimFrameRequested;
            }
            _ => (),
        }
    }

    pub fn get_geom(&self) -> &Rect {
        &self.layout_ctx.geom[self.id]
    }
}

impl<'a> Deref for ListenerCtx<'a> {
    type Target = Ui;

    fn deref(&self) -> &Ui {
        self.inner
    }
}

impl<'a> DerefMut for ListenerCtx<'a> {
    fn deref_mut(&mut self) -> &mut Ui {
        self.inner
    }
}

impl<'a> ListenerCtx<'a> {
    /// Bubble a poke action up the widget hierarchy, until a widget handles it.
    ///
    /// Returns true if any widget handled the action.
    pub fn poke_up<A: Any>(&mut self, payload: &mut A) -> bool {
        let mut node = self.id;
        loop {
            let parent = self.graph.parent[node];
            if parent == node {
                return false;
            }
            node = parent;
            if self.poke(node, payload) {
                return true;
            }
        }
    }

    /// Request the window to be closed.
    pub fn close(&mut self) {
        self.layout_ctx.handle.close();
    }

    pub fn file_dialog(
        &mut self,
        ty: FileDialogType,
        options: FileDialogOptions,
    ) -> Result<OsString, Error> {
        let result = self.layout_ctx.handle.file_dialog(ty, options)?;
        Ok(result)
    }
}

impl<'a, 'b> PaintCtx<'a, 'b> {
    /// Determine whether this widget is the active one.
    pub fn is_active(&self) -> bool {
        self.is_active
    }

    /// Determine whether this widget is hot.
    pub fn is_hot(&self) -> bool {
        self.is_hot
    }

    /// Determine whether this widget is focused.
    pub fn is_focused(&self) -> bool {
        self.is_focused
    }
}

impl WinHandler for UiMain {
    fn connect(&self, handle: &WindowHandle) {
        let mut state = self.state.borrow_mut();
        state.layout_ctx.handle = handle.clone();

        // Dispatch events; this is mostly to add listeners.
        state.dispatch_events();
    }

    fn paint(&self, paint_ctx: &mut Piet) -> bool {
        let mut state = self.state.borrow_mut();
        state.anim_frame();
        {
            paint_ctx.clear(BACKGROUND_COLOR);
        }
        let root = state.graph.root;
        let bc = BoxConstraints::tight(state.inner.layout_ctx.size);

        // TODO: be lazier about relayout
        state.layout(&bc, root);
        state.paint(paint_ctx, root);
        match state.layout_ctx.anim_state {
            AnimState::AnimFrameRequested => true,
            _ => {
                state.layout_ctx.anim_state = AnimState::Idle;
                state.layout_ctx.prev_paint_time = None;
                false
            }
        }
    }

    fn command(&self, id: u32) {
        // TODO: plumb through to client
        let mut state = self.state.borrow_mut();
        state.handle_command(id);
    }

    fn key_down(&self, event: KeyEvent) -> bool {
        let mut state = self.state.borrow_mut();
        state.handle_key_down(&event)
    }

    fn key_up(&self, event: KeyEvent) {
        let mut state = self.state.borrow_mut();
        state.handle_key_up(&event);
    }

    fn mouse_wheel(&self, dy: i32, mods: KeyModifiers) {
        let mut state = self.state.borrow_mut();
        state.handle_scroll(&window::ScrollEvent {
            dx: 0.0,
            dy: dy as f64,
            mods,
        });
    }

    fn mouse_hwheel(&self, dx: i32, mods: KeyModifiers) {
        let mut state = self.state.borrow_mut();
        state.handle_scroll(&window::ScrollEvent {
            dx: dx as f64,
            dy: 0.0,
            mods,
        });
    }

    fn mouse_move(&self, event: &window::MouseEvent) {
        let mut state = self.state.borrow_mut();
        let (x, y) = state.layout_ctx.handle.pixels_to_px_xy(event.x, event.y);
        let pos = Point::new(x as f64, y as f64);
        state.mouse_move(pos);
    }

    fn mouse(&self, event: &window::MouseEvent) {
        //println!("mouse {:?}", event);
        let mut state = self.state.borrow_mut();
        let (x, y) = state.layout_ctx.handle.pixels_to_px_xy(event.x, event.y);
        let pos = Point::new(x as f64, y as f64);
        // TODO: detect multiple clicks and pass that down
        state.mouse(pos, event);
    }

    fn destroy(&self) {
        Application::quit();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn size(&self, width: u32, height: u32) {
        let mut state = self.state.borrow_mut();
        let dpi = state.layout_ctx.handle.get_dpi() as f64;
        let scale = 96.0 / dpi;
        state.inner.layout_ctx.size = Size::new(width as f64 * scale, height as f64 * scale);
    }
}
