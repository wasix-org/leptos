#![forbid(unsafe_code)]
use crate::{
    hydration::SharedContext,
    node::{NodeId, ReactiveNode, ReactiveNodeState, ReactiveNodeType},
    AnyComputation, AnyResource, Effect, Memo, MemoState, ReadSignal,
    ResourceId, ResourceState, RwSignal, Scope, ScopeDisposer, ScopeId,
    ScopeProperty, SerializableResource, StoredValueId, Trigger,
    UnserializableResource, WriteSignal, SpecialNonReactiveZone,
};
use cfg_if::cfg_if;
use core::hash::BuildHasherDefault;
use futures::stream::FuturesUnordered;
use indexmap::IndexSet;
use rustc_hash::{FxHashMap, FxHasher};
use slotmap::{SecondaryMap, SlotMap, SparseSecondaryMap};
use std::{
    any::{Any, TypeId},
    cell::{Cell, RefCell},
    fmt::Debug,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    rc::Rc,
};

pub(crate) type PinnedFuture<T> = Pin<Box<dyn Future<Output = T>>>;

cfg_if! {
    if #[cfg(any(feature = "csr", feature = "hydrate"))] {
        thread_local! {
            pub(crate) static RUNTIME: Runtime = Runtime::new();
        }
    } else {
        thread_local! {
            pub(crate) static RUNTIMES: RefCell<SlotMap<RuntimeId, Runtime>> = Default::default();

            pub(crate) static CURRENT_RUNTIME: Cell<Option<RuntimeId>> = Default::default();
        }
    }
}

type FxIndexSet<T> = IndexSet<T, BuildHasherDefault<FxHasher>>;

// The data structure that owns all the signals, memos, effects,
// and other data included in the reactive system.
#[derive(Default)]
pub(crate) struct Runtime {
    pub shared_context: RefCell<SharedContext>,
    pub owner: Cell<Option<NodeId>>,
    pub observer: Cell<Option<NodeId>>,
    #[allow(clippy::type_complexity)]
    pub on_cleanups:
        RefCell<SparseSecondaryMap<NodeId, Vec<Box<dyn FnOnce()>>>>,
    pub stored_values: RefCell<SlotMap<StoredValueId, Rc<RefCell<dyn Any>>>>,
    pub nodes: RefCell<SlotMap<NodeId, ReactiveNode>>,
    pub node_subscribers:
        RefCell<SecondaryMap<NodeId, RefCell<FxIndexSet<NodeId>>>>,
    pub node_sources:
        RefCell<SecondaryMap<NodeId, RefCell<FxIndexSet<NodeId>>>>,
    pub node_owners: RefCell<SecondaryMap<NodeId, NodeId>>,
    pub node_properties:
        RefCell<SparseSecondaryMap<NodeId, Vec<ScopeProperty>>>,
    #[allow(clippy::type_complexity)]
    pub contexts:
        RefCell<SparseSecondaryMap<NodeId, FxHashMap<TypeId, Box<dyn Any>>>>,
    pub pending_effects: RefCell<Vec<NodeId>>,
    pub resources: RefCell<SlotMap<ResourceId, AnyResource>>,
    pub batching: Cell<bool>,
}

// This core Runtime impl block handles all the work of marking and updating
// the reactive graph.
//
// In terms of concept and algorithm, this reactive-system implementation
// is significantly inspired by Reactively (https://github.com/modderme123/reactively)
impl Runtime {
    #[inline(always)]
    pub(crate) fn current() -> RuntimeId {
        cfg_if! {
            if #[cfg(any(feature = "csr", feature = "hydrate"))] {
                Default::default()
            } else {
                CURRENT_RUNTIME.with(|id| id.get()).unwrap_or_default()
            }
        }
    }

    #[cfg(not(any(feature = "csr", feature = "hydrate")))]
    #[inline(always)]
    pub(crate) fn set_runtime(id: Option<RuntimeId>) {
         CURRENT_RUNTIME.with(|curr| curr.set(id))
    }

    pub(crate) fn update_if_necessary(&self, node_id: NodeId) {
        if self.current_state(node_id) == ReactiveNodeState::Check {
            let sources = {
                let sources = self.node_sources.borrow();

                // rather than cloning the entire FxIndexSet, only allocate a `Vec` for the node ids
                sources.get(node_id).map(|n| {
                    let sources = n.borrow();
                    // in case Vec::from_iterator specialization doesn't work, do it manually
                    let mut sources_vec = Vec::with_capacity(sources.len());
                    sources_vec.extend(sources.iter().cloned());
                    sources_vec
                })
            };

            for source in sources.into_iter().flatten() {
                self.update_if_necessary(source);
                if self.current_state(node_id) >= ReactiveNodeState::Dirty {
                    // as soon as a single parent has marked us dirty, we can
                    // stop checking them to avoid over-re-running
                    break;
                }
            }
        }

        // if we're dirty at this point, update
        if self.current_state(node_id) >= ReactiveNodeState::Dirty {
            // first, run our cleanups, if any
            if let Some(cleanups) =
                self.on_cleanups.borrow_mut().remove(node_id)
            {
                for cleanup in cleanups {
                    cleanup();
                }
            }

            // dispose of any of our properties
            let properties =
                { self.node_properties.borrow_mut().remove(node_id) };
            if let Some(properties) = properties {
                let mut nodes = self.nodes.borrow_mut();
                let mut cleanups = self.on_cleanups.borrow_mut();
                for property in properties {
                    self.cleanup_property(property, &mut nodes, &mut cleanups);
                }
            }

            // now, update the value
            self.update(node_id);
        }

        // now we're clean
        self.mark_clean(node_id);
    }

    pub(crate) fn update(&self, node_id: NodeId) {
        //crate::macros::debug_warn!("updating {node_id:?}");
        let node = {
            let nodes = self.nodes.borrow();
            nodes.get(node_id).cloned()
        };

        if let Some(node) = node {
            // memos and effects rerun
            // signals simply have their value
            let changed = match node.node_type {
                ReactiveNodeType::Signal | ReactiveNodeType::Trigger => true,
                ReactiveNodeType::Memo { ref f }
                | ReactiveNodeType::Effect { ref f } => {
                    let value = node.value();
                    // set this node as the observer
                    self.with_observer(node_id, move || {
                        // clean up sources of this memo/effect
                        self.cleanup_sources(node_id);

                        f.run(value)
                    })
                }
            };

            // mark children dirty
            if changed {
                let subs = self.node_subscribers.borrow();

                if let Some(subs) = subs.get(node_id) {
                    let mut nodes = self.nodes.borrow_mut();
                    for sub_id in subs.borrow().iter() {
                        if let Some(sub) = nodes.get_mut(*sub_id) {
                            //crate::macros::debug_warn!(
                            //    "update is marking {sub_id:?} dirty"
                            //);
                            sub.state = ReactiveNodeState::Dirty;
                        }
                    }
                }
            }

            // mark clean
            self.mark_clean(node_id);
        }
    }

    pub(crate) fn cleanup_property(
        &self,
        property: ScopeProperty,
        nodes: &mut SlotMap<NodeId, ReactiveNode>,
        cleanups: &mut SparseSecondaryMap<NodeId, Vec<Box<dyn FnOnce()>>>,
    ) {
        // for signals, triggers, memos, effects, shared node cleanup
        match property {
            ScopeProperty::Signal(node)
            | ScopeProperty::Trigger(node)
            | ScopeProperty::Effect(node) => {
                // clean up all children
                let properties =
                    { self.node_properties.borrow_mut().remove(node) };
                for property in properties.into_iter().flatten() {
                    self.cleanup_property(property, nodes, cleanups);
                }

                // run all cleanups for this node
                for cleanup in cleanups.remove(node).into_iter().flatten() {
                    cleanup();
                }

                // each of the subs needs to remove the node from its dependencies
                // so that it doesn't try to read the (now disposed) signal
                let subs = self.node_subscribers.borrow_mut().remove(node);

                if let Some(subs) = subs {
                    let source_map = self.node_sources.borrow();
                    for effect in subs.borrow().iter() {
                        if let Some(effect_sources) = source_map.get(*effect) {
                            effect_sources.borrow_mut().remove(&node);
                        }
                    }
                }

                // no longer needs to track its sources
                self.node_sources.borrow_mut().remove(node);

                // remove the node from the graph
                nodes.remove(node);
            }
            ScopeProperty::Resource(id) => {
                self.resources.borrow_mut().remove(id);
            }
            ScopeProperty::StoredValue(id) => {
                self.stored_values.borrow_mut().remove(id);
            }
        }
    }

    pub(crate) fn cleanup_sources(&self, node_id: NodeId) {
        let sources = self.node_sources.borrow();
        if let Some(sources) = sources.get(node_id) {
            let subs = self.node_subscribers.borrow();
            for source in sources.borrow().iter() {
                if let Some(source) = subs.get(*source) {
                    source.borrow_mut().remove(&node_id);
                }
            }
        }
    }

    fn current_state(&self, node: NodeId) -> ReactiveNodeState {
        match self.nodes.borrow().get(node) {
            None => ReactiveNodeState::Clean,
            Some(node) => node.state,
        }
    }

    fn with_observer<T>(&self, observer: NodeId, f: impl FnOnce() -> T) -> T {
        // take previous observer and owner
        let prev_observer = self.observer.take();
        let prev_owner = self.owner.take();

        self.owner.set(Some(observer));
        self.observer.set(Some(observer));
        let v = f();
        self.observer.set(prev_observer);
        self.owner.set(prev_owner);
        v
    }

    fn mark_clean(&self, node: NodeId) {
        //crate::macros::debug_warn!("marking {node:?} clean");
        let mut nodes = self.nodes.borrow_mut();
        if let Some(node) = nodes.get_mut(node) {
            node.state = ReactiveNodeState::Clean;
        }
    }

    #[allow(clippy::await_holding_refcell_ref)] // not using this part of ouroboros
    pub(crate) fn mark_dirty(&self, node: NodeId) {
        //crate::macros::debug_warn!("marking {node:?} dirty");
        let mut nodes = self.nodes.borrow_mut();

        if let Some(current_node) = nodes.get_mut(node) {
            if current_node.state == ReactiveNodeState::DirtyMarked {
                return;
            }

            let mut pending_effects = self.pending_effects.borrow_mut();
            let subscribers = self.node_subscribers.borrow();
            let current_observer = self.observer.get();

            // mark self dirty
            Runtime::mark(
                node,
                current_node,
                ReactiveNodeState::Dirty,
                &mut pending_effects,
                current_observer,
            );

            /*
             * Depth-first DAG traversal that uses a stack of iterators instead of
             * buffering the entire to-visit list. Visited nodes are either marked as
             * `Check` or `DirtyMarked`.
             *
             * Because `RefCell`, borrowing the iterators all at once is difficult,
             * so a self-referential struct is used instead. ouroboros produces safe
             * code, but it would not be recommended to use this outside of this
             * algorithm.
             */

            #[ouroboros::self_referencing]
            struct RefIter<'a> {
                set: std::cell::Ref<'a, FxIndexSet<NodeId>>,

                // Boxes the iterator internally
                #[borrows(set)]
                #[covariant]
                iter: indexmap::set::Iter<'this, NodeId>,
            }

            /// Due to the limitations of ouroboros, we cannot borrow the
            /// stack and iter simultaneously, or directly within the loop,
            /// therefore this must be used to command the outside scope
            /// of what to do.
            enum IterResult<'a> {
                Continue,
                Empty,
                NewIter(RefIter<'a>),
            }

            let mut stack = Vec::new();

            if let Some(children) = subscribers.get(node) {
                stack.push(RefIter::new(children.borrow(), |children| {
                    children.iter()
                }));
            }

            while let Some(iter) = stack.last_mut() {
                let res = iter.with_iter_mut(|iter| {
                    let Some(mut child) = iter.next().copied() else {
                        return IterResult::Empty;
                    };

                    while let Some(node) = nodes.get_mut(child) {
                        if node.state == ReactiveNodeState::Check
                            || node.state == ReactiveNodeState::DirtyMarked
                        {
                            return IterResult::Continue;
                        }

                        Runtime::mark(
                            child,
                            node,
                            ReactiveNodeState::Check,
                            &mut pending_effects,
                            current_observer,
                        );

                        if let Some(children) = subscribers.get(child) {
                            let children = children.borrow();

                            if !children.is_empty() {
                                // avoid going through an iterator in the simple psuedo-recursive case
                                if children.len() == 1 {
                                    child = children[0];
                                    continue;
                                }

                                return IterResult::NewIter(RefIter::new(
                                    children,
                                    |children| children.iter(),
                                ));
                            }
                        }

                        break;
                    }

                    IterResult::Continue
                });

                match res {
                    IterResult::Continue => continue,
                    IterResult::NewIter(iter) => stack.push(iter),
                    IterResult::Empty => {
                        stack.pop();
                    }
                }
            }
        }
    }

    #[inline(always)] // small function, used in hot loop
    fn mark(
        //nodes: &mut SlotMap<NodeId, ReactiveNode>,
        node_id: NodeId,
        node: &mut ReactiveNode,
        level: ReactiveNodeState,
        pending_effects: &mut Vec<NodeId>,
        current_observer: Option<NodeId>,
    ) {
        //crate::macros::debug_warn!("marking {node_id:?} {level:?}");
        if level > node.state {
            node.state = level;
        }

        if matches!(node.node_type, ReactiveNodeType::Effect { .. } if current_observer != Some(node_id))
        {
            //crate::macros::debug_warn!("pushing effect {node_id:?}");
            //debug_assert!(!pending_effects.contains(&node_id));
            pending_effects.push(node_id)
        }

        if node.state == ReactiveNodeState::Dirty {
            node.state = ReactiveNodeState::DirtyMarked;
        }
    }

    pub(crate) fn run_effects(&self) {
        if !self.batching.get() {
            let effects = self.pending_effects.take();
            for effect_id in effects {
                self.update_if_necessary(effect_id);
            }
        }
    }

    pub(crate) fn dispose_node(&self, node: NodeId) {
        self.node_sources.borrow_mut().remove(node);
        self.node_subscribers.borrow_mut().remove(node);
        self.nodes.borrow_mut().remove(node);
    }

    #[track_caller]
    pub(crate) fn register_property(
        &self,
        property: ScopeProperty,
        #[cfg(debug_assertions)] defined_at: &'static std::panic::Location<
            'static,
        >,
    ) {
        let mut properties = self.node_properties.borrow_mut();
        if let Some(owner) = self.owner.get() {
            if let Some(entry) = properties.entry(owner) {
                let entry = entry.or_default();
                entry.push(property);
            }

            if let Some(node) = property.to_node_id() {
                let mut owners = self.node_owners.borrow_mut();
                owners.insert(node, owner);
            }
        } else {
            crate::macros::debug_warn!(
                "At {defined_at}, you are creating a reactive value outside \
                 the reactive root.",
            );
        }
    }

    pub(crate) fn get_context<T: Clone + 'static>(
        &self,
        node: NodeId,
        ty: TypeId,
    ) -> Option<T> {
        let contexts = self.contexts.borrow();

        let context = contexts.get(node);
        let local_value = context.and_then(|context| {
            context
                .get(&ty)
                .and_then(|val| val.downcast_ref::<T>())
                .cloned()
        });
        match local_value {
            Some(val) => Some(val),
            None => self
                .node_owners
                .borrow()
                .get(node)
                .and_then(|parent| self.get_context(*parent, ty)),
        }
    }

    #[cfg_attr(
        any(debug_assertions, features = "ssr"),
        instrument(level = "trace", skip_all,)
    )]
    #[track_caller]
    pub(crate) fn push_scope_property(&self, prop: ScopeProperty) {
        #[cfg(debug_assertions)]
        let defined_at = std::panic::Location::caller();
        self.register_property(
            prop,
            #[cfg(debug_assertions)]
            defined_at,
        );
    }
}

impl Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime").finish()
    }
}
/// Get the selected runtime from the thread-local set of runtimes. On the server,
/// this will return the correct runtime. In the browser, there should only be one runtime.
#[cfg_attr(
    any(debug_assertions, feature = "ssr"),
    instrument(level = "trace", skip_all,)
)]
#[inline(always)] // it monomorphizes anyway
pub(crate) fn with_runtime<T>(
    id: RuntimeId,
    f: impl FnOnce(&Runtime) -> T,
) -> Result<T, ()> {
    // in the browser, everything should exist under one runtime
    cfg_if! {
        if #[cfg(any(feature = "csr", feature = "hydrate"))] {
            _ = id;
            Ok(RUNTIME.with(|runtime| f(runtime)))
        } else {
            RUNTIMES.with(|runtimes| {
                let runtimes = runtimes.borrow();
                match runtimes.get(id) {
                    None => Err(()),
                    Some(runtime) => Ok(f(runtime))
                }
            })
        }
    }
}

#[doc(hidden)]
#[must_use = "Runtime will leak memory if Runtime::dispose() is never called."]
/// Creates a new reactive [`Runtime`]. This should almost always be handled by the framework.
pub fn create_runtime() -> RuntimeId {
    cfg_if! {
        if #[cfg(any(feature = "csr", feature = "hydrate"))] {
            Default::default()
        } else {
            RUNTIMES.with(|runtimes| runtimes.borrow_mut().insert(Runtime::new()))
        }
    }
}

#[cfg(not(any(feature = "csr", feature = "hydrate")))]
slotmap::new_key_type! {
    /// Unique ID assigned to a Runtime.
    pub struct RuntimeId;
}

/// Unique ID assigned to a Runtime.
#[cfg(any(feature = "csr", feature = "hydrate"))]
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeId;

impl RuntimeId {
    // TODO remove this
    /// Removes the runtime, disposing all its child [`Scope`](crate::Scope)s.
    pub fn dispose(self) {
        cfg_if! {
            if #[cfg(not(any(feature = "csr", feature = "hydrate")))] {
                let runtime = RUNTIMES.with(move |runtimes| runtimes.borrow_mut().remove(self));
                drop(runtime);
            }
        }
    }

    // TODO remove this
    pub(crate) fn raw_scope_and_disposer(self) -> (Scope, ScopeDisposer) {
        with_runtime(self, |runtime| {
            let scope = Scope {
                runtime: self,
                id: Default::default(),
            };
            let disposer = ScopeDisposer(scope);
            (scope, disposer)
        })
        .expect(
            "tried to create raw scope in a runtime that has already been \
             disposed",
        )
    }

    // TODO remove this
    pub(crate) fn raw_scope_and_disposer_with_parent(
        self,
        parent: Option<Scope>,
    ) -> (Scope, ScopeDisposer) {
        with_runtime(self, |runtime| {
            let scope = Scope {
                runtime: self,
                id: Default::default(),
            };
            let disposer = ScopeDisposer(scope);
            (scope, disposer)
        })
        .expect("tried to crate scope in a runtime that has been disposed")
    }

    // TODO remove this
    #[inline(always)]
    pub(crate) fn run_scope_undisposed<T>(
        self,
        f: impl FnOnce(Scope) -> T,
        parent: Option<Scope>,
    ) -> (T, ScopeId, ScopeDisposer) {
        let (scope, disposer) = self.raw_scope_and_disposer_with_parent(parent);

        (f(scope), scope.id, disposer)
    }

    // TODO remove this
    #[inline(always)]
    pub(crate) fn run_scope<T>(
        self,
        f: impl FnOnce(Scope) -> T,
        parent: Option<Scope>,
    ) -> T {
        let (ret, _, disposer) = self.run_scope_undisposed(f, parent);
        disposer.dispose();
        ret
    }

    #[track_caller]
    #[inline(always)] // only because it's placed here to fit in with the other create methods
    pub(crate) fn create_trigger(self) -> Trigger {
        let id = with_runtime(self, |runtime| {
            let id = runtime.nodes.borrow_mut().insert(ReactiveNode {
                value: None,
                state: ReactiveNodeState::Clean,
                node_type: ReactiveNodeType::Trigger,
            });
            runtime.push_scope_property(ScopeProperty::Trigger(id));
            id
        })
        .expect(
            "tried to create a trigger in a runtime that has been disposed",
        );

        Trigger {
            id,
            runtime: self,
            #[cfg(debug_assertions)]
            defined_at: std::panic::Location::caller(),
        }
    }

    pub(crate) fn create_concrete_signal(
        self,
        value: Rc<RefCell<dyn Any>>,
    ) -> NodeId {
        with_runtime(self, |runtime| {
            let id = runtime.nodes.borrow_mut().insert(ReactiveNode {
                value: Some(value),
                state: ReactiveNodeState::Clean,
                node_type: ReactiveNodeType::Signal,
            });
            runtime.push_scope_property(ScopeProperty::Signal(id));
            id
        })
        .expect("tried to create a signal in a runtime that has been disposed")
    }

    #[track_caller]
    #[inline(always)]
    pub(crate) fn create_signal<T>(
        self,
        value: T,
    ) -> (ReadSignal<T>, WriteSignal<T>)
    where
        T: Any + 'static,
    {
        let id = self.create_concrete_signal(
            Rc::new(RefCell::new(value)) as Rc<RefCell<dyn Any>>
        );

        (
            ReadSignal {
                runtime: self,
                id,
                ty: PhantomData,
                #[cfg(any(debug_assertions, feature = "ssr"))]
                defined_at: std::panic::Location::caller(),
            },
            WriteSignal {
                runtime: self,
                id,
                ty: PhantomData,
                #[cfg(any(debug_assertions, feature = "ssr"))]
                defined_at: std::panic::Location::caller(),
            },
        )
    }

    #[track_caller]
    #[inline(always)]
    pub(crate) fn create_rw_signal<T>(self, value: T) -> RwSignal<T>
    where
        T: Any + 'static,
    {
        let id = self.create_concrete_signal(
            Rc::new(RefCell::new(value)) as Rc<RefCell<dyn Any>>
        );
        //crate::macros::debug_warn!(
        //    "created RwSignal {id:?} at {:?}",
        //    std::panic::Location::caller()
        //);
        RwSignal {
            runtime: self,
            id,
            ty: PhantomData,
            #[cfg(any(debug_assertions, feature = "ssr"))]
            defined_at: std::panic::Location::caller(),
        }
    }

    pub(crate) fn create_concrete_effect(
        self,
        value: Rc<RefCell<dyn Any>>,
        effect: Rc<dyn AnyComputation>,
    ) -> NodeId {
        with_runtime(self, |runtime| {
            let id = runtime.nodes.borrow_mut().insert(ReactiveNode {
                value: Some(Rc::clone(&value)),
                state: ReactiveNodeState::Dirty,
                node_type: ReactiveNodeType::Effect {
                    f: Rc::clone(&effect),
                },
            });
            runtime.push_scope_property(ScopeProperty::Effect(id));
            id
        })
        .expect("tried to create an effect in a runtime that has been disposed")
    }

    pub(crate) fn create_concrete_memo(
        self,
        value: Rc<RefCell<dyn Any>>,
        computation: Rc<dyn AnyComputation>,
    ) -> NodeId {
        with_runtime(self, |runtime| {
            let id = runtime.nodes.borrow_mut().insert(ReactiveNode {
                value: Some(value),
                // memos are lazy, so are dirty when created
                // will be run the first time we ask for it
                state: ReactiveNodeState::Dirty,
                node_type: ReactiveNodeType::Memo { f: computation },
            });
            runtime.push_scope_property(ScopeProperty::Effect(id));
            id
        })
        .expect("tried to create a memo in a runtime that has been disposed")
    }

    #[track_caller]
    #[inline(always)]
    pub(crate) fn create_effect<T>(
        self,
        f: impl Fn(Option<T>) -> T + 'static,
    ) -> NodeId
    where
        T: Any + 'static,
    {
        self.create_concrete_effect(
            Rc::new(RefCell::new(None::<T>)),
            Rc::new(Effect {
                f,
                ty: PhantomData,
                #[cfg(any(debug_assertions, feature = "ssr"))]
                defined_at: std::panic::Location::caller(),
            }),
        )
    }

    #[track_caller]
    #[inline(always)]
    pub(crate) fn create_memo<T>(
        self,
        f: impl Fn(Option<&T>) -> T + 'static,
    ) -> Memo<T>
    where
        T: PartialEq + Any + 'static,
    {
        Memo {
            runtime: self,
            id: self.create_concrete_memo(
                Rc::new(RefCell::new(None::<T>)),
                Rc::new(MemoState {
                    f,
                    t: PhantomData,
                    #[cfg(any(debug_assertions, feature = "ssr"))]
                    defined_at: std::panic::Location::caller(),
                }),
            ),
            ty: PhantomData,
            #[cfg(any(debug_assertions, feature = "ssr"))]
            defined_at: std::panic::Location::caller(),
        }
    }
}

impl Runtime {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn create_unserializable_resource(
        &self,
        state: Rc<dyn UnserializableResource>,
    ) -> ResourceId {
        self.resources
            .borrow_mut()
            .insert(AnyResource::Unserializable(state))
    }

    pub(crate) fn create_serializable_resource(
        &self,
        state: Rc<dyn SerializableResource>,
    ) -> ResourceId {
        self.resources
            .borrow_mut()
            .insert(AnyResource::Serializable(state))
    }
    #[cfg_attr(
        any(debug_assertions, feature = "ssr"),
        instrument(level = "trace", skip_all,)
    )]
    pub(crate) fn resource<S, T, U>(
        &self,
        id: ResourceId,
        f: impl FnOnce(&ResourceState<S, T>) -> U,
    ) -> U
    where
        S: 'static,
        T: 'static,
    {
        let resources = self.resources.borrow();
        let res = resources.get(id);
        if let Some(res) = res {
            let res_state = match res {
                AnyResource::Unserializable(res) => res.as_any(),
                AnyResource::Serializable(res) => res.as_any(),
            }
            .downcast_ref::<ResourceState<S, T>>();

            if let Some(n) = res_state {
                f(n)
            } else {
                panic!(
                    "couldn't convert {id:?} to ResourceState<{}, {}>",
                    std::any::type_name::<S>(),
                    std::any::type_name::<T>(),
                );
            }
        } else {
            panic!("couldn't locate {id:?}");
        }
    }

    /// Returns IDs for all [resources](crate::Resource) found on any scope.
    pub(crate) fn all_resources(&self) -> Vec<ResourceId> {
        self.resources
            .borrow()
            .iter()
            .map(|(resource_id, _)| resource_id)
            .collect()
    }

    /// Returns IDs for all [resources](crate::Resource) found on any
    /// scope, pending from the server.
    pub(crate) fn pending_resources(&self) -> Vec<ResourceId> {
        self.resources
            .borrow()
            .iter()
            .filter_map(|(resource_id, res)| {
                if matches!(res, AnyResource::Serializable(_)) {
                    Some(resource_id)
                } else {
                    None
                }
            })
            .collect()
    }

    pub(crate) fn serialization_resolvers(
        &self,
    ) -> FuturesUnordered<PinnedFuture<(ResourceId, String)>> {
        let f = FuturesUnordered::new();
        for (id, resource) in self.resources.borrow().iter() {
            if let AnyResource::Serializable(resource) = resource {
                f.push(resource.to_serialization_resolver(id));
            }
        }
        f
    }

    /// Do not call on triggers
    pub(crate) fn get_value(
        &self,
        node_id: NodeId,
    ) -> Option<Rc<RefCell<dyn Any>>> {
        let signals = self.nodes.borrow();
        signals.get(node_id).map(|node| node.value())
    }
}

impl PartialEq for Runtime {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for Runtime {}

impl std::hash::Hash for Runtime {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::ptr::hash(&self, state);
    }
}

    /// Suspends reactive tracking while running the given function.
    ///
    /// This can be used to isolate parts of the reactive graph from one another.
    ///
    /// ```
    /// # use leptos_reactive::*;
    /// # run_scope(create_runtime(), |cx| {
    /// let (a, set_a) = create_signal(cx, 0);
    /// let (b, set_b) = create_signal(cx, 0);
    /// let c = create_memo(cx, move |_| {
    ///     // this memo will *only* update when `a` changes
    ///     a() + cx.untrack(move || b())
    /// });
    ///
    /// assert_eq!(c(), 0);
    /// set_a(1);
    /// assert_eq!(c(), 1);
    /// set_b(1);
    /// // hasn't updated, because we untracked before reading b
    /// assert_eq!(c(), 1);
    /// set_a(2);
    /// assert_eq!(c(), 3);
    ///
    /// # });
    /// ```
    #[cfg_attr(
        any(debug_assertions, features = "ssr"),
        instrument(level = "trace", skip_all,)
    )]
    #[inline(always)]
    pub fn untrack<T>(f: impl FnOnce() -> T) -> T {
        let runtime_id = Runtime::current();
        with_runtime(runtime_id, |runtime| {
            let untracked_result;

            SpecialNonReactiveZone::enter();

            let prev_observer =
                SetObserverOnDrop(runtime_id, runtime.observer.take());

            untracked_result = f();

            runtime.observer.set(prev_observer.1);
            std::mem::forget(prev_observer); // avoid Drop

            SpecialNonReactiveZone::exit();

            untracked_result
        })
        .expect(
            "tried to run untracked function in a runtime that has been \
             disposed",
        )
    }

    struct SetObserverOnDrop(RuntimeId, Option<NodeId>);

impl Drop for SetObserverOnDrop {
    fn drop(&mut self) {
        _ = with_runtime(self.0, |rt| {
            rt.observer.set(self.1);
        });
    }
}